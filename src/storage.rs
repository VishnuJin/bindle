use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{create_dir_all, File, OpenOptions};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::sync::RwLock;

/// The folder name for the invoices directory
const INVOICE_DIRECTORY: &str = "invoices";
/// The folder name for the parcels directory
const PARCEL_DIRECTORY: &str = "parcels";
const INVOICE_TOML: &str = "invoice.toml";
const PARCEL_DAT: &str = "parcel.dat";
const LABEL_TOML: &str = "label.toml";

type Result<T> = core::result::Result<T, StorageError>;

#[async_trait::async_trait]
pub trait Storage {
    /// This takes an invoice and creates it in storage.
    /// It must verify that each referenced box is present in storage. Any box that
    /// is not present must be returned in the list of IDs.
    async fn create_invoice(&self, inv: &super::Invoice) -> Result<Vec<super::Label>>;
    // Load an invoice and return it
    //
    // This will return an invoice if the bindle exists and is not yanked
    async fn get_invoice(&self, id: String) -> Result<super::Invoice>;
    // Load an invoice, even if it is yanked.
    async fn get_yanked_invoice(&self, id: String) -> Result<super::Invoice>;
    // Remove an invoice
    //
    // Because invoices are not necessarily stored using just one field on the invoice,
    // the entire invoice must be passed to the deletion command.
    async fn yank_invoice(&self, inv: &mut super::Invoice) -> Result<()>;
    async fn create_parcel<R: AsyncRead + Unpin + Send + Sync>(
        &self,
        label: &super::Label,
        data: &mut R,
    ) -> Result<()>;

    async fn get_parcel(&self, label: &crate::Label) -> Result<Box<dyn AsyncRead + Unpin>>;
    // Get the label for a parcel
    //
    // This reads the label from storage and then parses it into a Label object.
    async fn get_label(&self, parcel_id: &str) -> Result<crate::Label>;
}

/// StorageError describes the possible error states when storing and retrieving bindles.
#[derive(Error, Debug)]
pub enum StorageError {
    #[error("bindle is yanked")]
    Yanked,
    #[error("bindle cannot be created as yanked")]
    CreateYanked,
    #[error("resource not found")]
    NotFound,
    #[error("resource could not be loaded")]
    IO(#[from] std::io::Error),
    #[error("resource already exists")]
    Exists,

    // TODO: Investigate how to make this more helpful
    #[error("resource is malformed")]
    Malformed(#[from] toml::de::Error),
    #[error("resource cannot be stored")]
    Unserializable(#[from] toml::ser::Error),
}

/// A file system backend for storing and retriving bindles and parcles.
///
/// Given a root directory, FileStorage brings its own storage layout for keeping track
/// of Bindles.
///
/// A FileStorage needs a search engine implementation. When invoices are created or yanked,
/// the index will be updated.
pub struct FileStorage<T> {
    root: PathBuf,
    index: Arc<RwLock<T>>,
}

// Manual implementation for Clone due to derive putting a clone constraint on generic parameters
impl<T> Clone for FileStorage<T> {
    fn clone(&self) -> Self {
        FileStorage {
            root: self.root.clone(),
            index: self.index.clone(),
        }
    }
}

impl<T> FileStorage<T> {
    pub fn new<P: AsRef<Path>>(path: P, index: T) -> Self {
        FileStorage {
            root: path.as_ref().to_owned(),
            index: Arc::new(RwLock::new(index)),
        }
    }

    /// Create a standard name for an invoice
    ///
    /// This is designed to create a repeatable opaque name when given an invoice.
    fn canonical_invoice_name(&self, inv: &crate::Invoice) -> String {
        self.canonical_invoice_name_strings(inv.bindle.name.as_str(), inv.bindle.version.as_str())
    }

    /// Given a name and a version, this returns a repeatable name for an on-disk location.
    ///
    /// We don't typically want to store a bindle with its name and version number. This
    /// would impose both naming constraints on the bindle and security issues on the
    /// storage layout. So this function hashes the name/version data (which together
    /// MUST be unique in the system) and uses the resulting hash as the canonical
    /// name. The hash is guaranteed to be in the character set [a-zA-Z0-9].
    fn canonical_invoice_name_strings(&self, name: &str, version: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(name.as_bytes());
        hasher.update(version.as_bytes());
        let result = hasher.finalize();
        format!("{:x}", result)
    }

    /// Return the path to the invoice directory for a particular bindle.
    fn invoice_path(&self, invoice_id: &str) -> PathBuf {
        let mut path = self.root.join(INVOICE_DIRECTORY);
        path.push(invoice_id);
        path
    }
    /// Return the path for an invoice.toml for a particular bindle.
    fn invoice_toml_path(&self, invoice_id: &str) -> PathBuf {
        self.invoice_path(invoice_id).join(INVOICE_TOML)
    }
    /// Return the parcel-specific path for storing a parcel.
    fn parcel_path(&self, parcel_id: &str) -> PathBuf {
        let mut path = self.root.join(PARCEL_DIRECTORY);
        path.push(parcel_id);
        path
    }
    /// Return the path to a parcel.toml for a specific parcel.
    fn label_toml_path(&self, parcel_id: &str) -> PathBuf {
        self.parcel_path(parcel_id).join(LABEL_TOML)
    }
    /// Return the path to the parcel.dat file for the given box ID
    fn parcel_data_path(&self, parcel_id: &str) -> PathBuf {
        self.parcel_path(parcel_id).join(PARCEL_DAT)
    }
}

#[async_trait::async_trait]
impl<T: crate::search::Search + Send + Sync> Storage for FileStorage<T> {
    async fn create_invoice(&self, inv: &super::Invoice) -> Result<Vec<super::Label>> {
        // It is illegal to create a yanked invoice.
        if inv.yanked.unwrap_or(false) {
            return Err(StorageError::CreateYanked);
        }

        let invoice_cname = self.canonical_invoice_name(inv);
        let invoice_id = invoice_cname.as_str();

        // Create the base path if necessary
        let inv_path = self.invoice_path(invoice_id);
        if !inv_path.is_dir() {
            // If it exists and is a regular file, we have a problem
            if inv_path.is_file() {
                return Err(StorageError::Exists);
            }
            create_dir_all(inv_path).await?;
        }

        // Open the destination or error out if it already exists.
        let dest = self.invoice_toml_path(invoice_id);
        let mut out = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(dest)
            .await?;

        // Encode the invoice into a TOML object
        let data = toml::to_vec(inv)?;
        out.write_all(data.as_slice()).await?;

        // Attempt to update the index. Right now, we log an error if the index update
        // fails.
        {
            let mut lock = self.index.write().await;
            if let Err(e) = lock.index(&inv) {
                eprintln!("Error indexing {}: {}", invoice_id, e);
            }
        }

        // if there are no parcels, bail early
        if inv.parcels.is_none() {
            return Ok(vec![]);
        }

        // Note: this will not allocate
        let zero_vec = Vec::with_capacity(0);
        // Loop through the boxes and see what exists
        let missing = inv
            .parcels
            .as_ref()
            .unwrap_or(&zero_vec)
            .iter()
            .map(|k| async move {
                let parcel_path = self.parcel_path(k.label.sha256.as_str());
                // Stat k to see if it exists. If it does not exist or is not a directory, add it.
                let res = tokio::fs::metadata(parcel_path).await;
                match res {
                    Ok(stat) if !stat.is_dir() => Some(k.label.clone()),
                    Err(_e) => Some(k.label.clone()),
                    _ => None,
                }
            });

        Ok(futures::future::join_all(missing)
            .await
            .into_iter()
            .filter_map(|x| x)
            .collect())
    }
    async fn get_invoice(&self, id: String) -> Result<super::Invoice> {
        match self.get_yanked_invoice(id).await {
            Ok(inv) if !inv.yanked.unwrap_or(false) => Ok(inv),
            Err(e) => Err(e),
            _ => Err(StorageError::Yanked),
        }
    }
    async fn get_yanked_invoice(&self, id: String) -> Result<super::Invoice> {
        // TODO: Parse the id into an invoice name and version.
        let id_path = Path::new(&id);
        let parent = id_path.parent();
        if parent.is_none() {
            return Err(StorageError::NotFound);
        }

        let name = parent.unwrap().to_str().unwrap();

        let version_part = id_path.file_name();
        if version_part.is_none() {
            return Err(StorageError::NotFound);
        }
        let version = version_part.unwrap().to_str().unwrap();

        let invoice_cname = self.canonical_invoice_name_strings(name, version);
        let invoice_id = invoice_cname.as_str();

        // Now construct a path and read it
        let invoice_path = self.invoice_toml_path(invoice_id);

        // Open file
        let inv_toml = std::fs::read_to_string(invoice_path)?;

        // Parse
        let invoice: crate::Invoice = toml::from_str(inv_toml.as_str())?;

        // Return object
        Ok(invoice)
    }
    async fn yank_invoice(&self, inv: &mut super::Invoice) -> Result<()> {
        let invoice_cname = self.canonical_invoice_name(inv);
        let invoice_id = invoice_cname.as_str();
        // Load the invoice and mark it as yanked.
        inv.yanked = Some(true);

        // Attempt to update the index. Right now, we log an error if the index update
        // fails.
        {
            let mut lock = self.index.write().await;
            if let Err(e) = lock.index(&inv) {
                eprintln!("Error indexing {}: {}", invoice_id, e);
            }
        }

        // Open the destination or error out if it already exists.
        let dest = self.invoice_toml_path(invoice_id);

        // Encode the invoice into a TOML object
        let data = toml::to_vec(inv)?;
        // NOTE: Right now, this just force-overwites the existing invoice. We are assuming
        // that the bindle has already been confirmed to be present. However, we have not
        // ensured that here. So it is theoretically possible (if get_invoice was not used)
        // to build the invoice) that this could _create_ a new file. We could probably change
        // this behavior with OpenOptions.

        std::fs::write(dest, data)?;
        Ok(())
    }
    async fn create_parcel<R: AsyncRead + Unpin + Send + Sync>(
        &self,
        label: &super::Label,
        data: &mut R,
    ) -> Result<()> {
        let sha = label.sha256.as_str();
        // Test if a dir with that SHA exists. If so, this is an error.
        let par_path = self.parcel_path(sha);
        if par_path.is_file() {
            return Err(StorageError::Exists);
        }
        // Create box dir
        create_dir_all(par_path).await?;

        // Write data
        {
            let data_file = self.parcel_data_path(sha);
            let mut out = OpenOptions::new()
                .create_new(true)
                .write(true)
                .read(false)
                .open(data_file)
                .await?;

            tokio::io::copy(data, &mut out).await?;
        }

        // Write label
        {
            let dest = self.label_toml_path(sha);
            let mut out = OpenOptions::new()
                .create_new(true)
                .write(true)
                .read(true)
                .open(dest)
                .await?;

            let data = toml::to_vec(label)?;
            out.write_all(data.as_slice()).await?;
        }
        Ok(())
    }
    async fn get_parcel(&self, label: &crate::Label) -> Result<Box<dyn AsyncRead + Unpin>> {
        let name = self.parcel_data_path(label.sha256.as_str());
        let reader = File::open(name).await?;
        Ok(Box::new(reader))
    }

    async fn get_label(&self, parcel_id: &str) -> Result<crate::Label> {
        let label_path = self.label_toml_path(parcel_id);
        let label_toml = std::fs::read_to_string(label_path)?;
        let label: crate::Label = toml::from_str(label_toml.as_str())?;

        // Return object
        Ok(label)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Invoice;
    use std::io::SeekFrom;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_should_generate_paths() {
        let f = FileStorage::new("test", crate::search::StrictEngine::default());
        assert_eq!("test/invoices/123", f.invoice_path("123").to_string_lossy());
        assert_eq!(
            "test/invoices/123/invoice.toml",
            f.invoice_toml_path("123").to_string_lossy()
        );
        assert_eq!(
            "test/parcels/123".to_owned(),
            f.parcel_path("123").to_string_lossy()
        );
        assert_eq!(
            "test/parcels/123/label.toml".to_owned(),
            f.label_toml_path("123").to_string_lossy()
        );
        assert_eq!(
            "test/parcels/123/parcel.dat".to_owned(),
            f.parcel_data_path("123").to_string_lossy()
        );
    }

    #[tokio::test]
    async fn test_should_create_yank_invoice() {
        // Create a temporary directory
        let root = tempdir().unwrap();
        let mut inv = invoice_fixture();
        let store = FileStorage::new(
            root.path().to_owned(),
            crate::search::StrictEngine::default(),
        );
        let inv_cname = store.canonical_invoice_name(&inv);
        let inv_name = inv_cname.as_str();
        // Create an file
        let missing = store.create_invoice(&inv).await.unwrap();
        assert_eq!(3, missing.len());

        // Out-of-band read the invoice
        assert!(store.invoice_toml_path(inv_name).exists());

        // Yank the invoice
        store.yank_invoice(&mut inv).await.unwrap();

        // Make sure the invoice is yanked
        let inv2 = store
            .get_yanked_invoice(crate::invoice_to_name(&inv))
            .await
            .unwrap();
        assert!(inv2.yanked.unwrap_or(false));

        // Sanity check that this produces an error
        assert!(store
            .get_invoice(crate::invoice_to_name(&inv))
            .await
            .is_err());

        // Drop the temporary directory
        assert!(root.close().is_ok());
    }

    #[tokio::test]
    async fn test_should_reject_yanked_invoice() {
        // Create a temporary directory
        let root = tempdir().unwrap();
        let mut inv = invoice_fixture();
        inv.yanked = Some(true);
        let store = FileStorage::new(
            root.path().to_owned(),
            crate::search::StrictEngine::default(),
        );
        // Create an file
        assert!(store.create_invoice(&inv).await.is_err());
        assert!(root.close().is_ok());
    }

    #[tokio::test]
    async fn test_should_write_read_parcel() {
        let id = "abcdef1234567890987654321";
        let (label, mut data) = parcel_fixture(id).await;
        let root = tempdir().expect("create tempdir");
        let store = FileStorage::new(
            root.path().to_owned(),
            crate::search::StrictEngine::default(),
        );

        store
            .create_parcel(&label, &mut data)
            .await
            .expect("create parcel");

        // Now attempt to read just the label

        let label2 = store.get_label(id).await.expect("fetch label after saving");
        let mut data = String::new();
        store
            .get_parcel(&label2)
            .await
            .expect("load parcel data")
            .read_to_string(&mut data)
            .await
            .expect("read file into string");
        assert_eq!(data, "hello\n");
    }

    #[tokio::test]
    async fn test_should_store_and_retrieve_bindle() {
        let root = tempdir().expect("create tempdir");
        let store = FileStorage::new(
            root.path().to_owned(),
            crate::search::StrictEngine::default(),
        );

        // Store a parcel
        let id = "abcdef1234567890987654321";
        let (label, mut data) = parcel_fixture(id).await;
        let mut invoice = invoice_fixture();
        let inv_name = crate::invoice_to_name(&invoice);

        let parcel = crate::Parcel {
            label: label.clone(),
            conditions: None,
        };
        invoice.parcels = Some(vec![parcel]);

        store
            .create_parcel(&label, &mut data)
            .await
            .expect("stored the parcel");

        // Store an invoice that points to that parcel

        store.create_invoice(&invoice).await.expect("create parcel");

        // Get the bindle
        let inv = store
            .get_invoice(inv_name)
            .await
            .expect("get the invoice we just stored");

        let first_parcel = inv
            .parcels
            .expect("parsel vector")
            .pop()
            .expect("got a parcel");
        assert_eq!(first_parcel.label.name, "foo.toml".to_owned())
    }

    async fn parcel_fixture(id: &str) -> (crate::Label, tokio::fs::File) {
        let data = tempfile::tempfile().unwrap();
        let mut data = File::from_std(data);
        data.write_all("hello\n".as_bytes())
            .await
            .expect("unable to write test data");
        data.flush().await.expect("unable to flush the test file");
        data.seek(SeekFrom::Start(0))
            .await
            .expect("unable to reset read pointer to head");
        (
            crate::Label {
                sha256: id.to_owned(),
                media_type: "text/toml".to_owned(),
                name: "foo.toml".to_owned(),
                size: Some(6),
                annotations: None,
            },
            data,
        )
    }

    fn invoice_fixture() -> Invoice {
        let labels = vec![
            crate::Label {
                sha256: "abcdef1234567890987654321".to_owned(),
                media_type: "text/toml".to_owned(),
                name: "foo.toml".to_owned(),
                size: Some(101),
                annotations: None,
            },
            crate::Label {
                sha256: "bbcdef1234567890987654321".to_owned(),
                media_type: "text/toml".to_owned(),
                name: "foo2.toml".to_owned(),
                size: Some(101),
                annotations: None,
            },
            crate::Label {
                sha256: "cbcdef1234567890987654321".to_owned(),
                media_type: "text/toml".to_owned(),
                name: "foo3.toml".to_owned(),
                size: Some(101),
                annotations: None,
            },
        ];

        Invoice {
            bindle_version: crate::BINDLE_VERSION_1.to_owned(),
            yanked: None,
            annotations: None,
            bindle: crate::BindleSpec {
                name: "foo".to_owned(),
                description: Some("bar".to_owned()),
                version: "v1.2.3".to_owned(),
                authors: Some(vec!["m butcher".to_owned()]),
            },
            parcels: Some(
                labels
                    .iter()
                    .map(|l| crate::Parcel {
                        label: l.clone(),
                        conditions: None,
                    })
                    .collect(),
            ),
            group: None,
        }
    }
}
