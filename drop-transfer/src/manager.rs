use std::{
    collections::{hash_map::Entry, HashMap},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::{mpsc::UnboundedSender, Mutex};
use uuid::Uuid;

use crate::{
    file::FileSubPath,
    transfer::{IncomingTransfer, OutgoingTransfer},
    ws::{client::ClientReq, server::ServerReq},
};

pub struct IncomingState {
    pub xfer: Arc<IncomingTransfer>,
    pub conn: Option<UnboundedSender<ServerReq>>,
    pub dir_mappings: DirMapping,
}

pub struct OutgoingState {
    pub xfer: Arc<OutgoingTransfer>,
    pub conn: Option<UnboundedSender<ClientReq>>,
}

/// Transfer manager is responsible for keeping track of all ongoing or pending
/// transfers and their status
#[derive(Default)]
pub struct TransferManager {
    pub incoming: Mutex<HashMap<Uuid, IncomingState>>,
    pub outgoing: Mutex<HashMap<Uuid, OutgoingState>>,
}

#[derive(Default)]
pub struct DirMapping {
    mappings: HashMap<PathBuf, String>,
}

impl DirMapping {
    /// This function composes the final path for the file.
    /// For ordinary files (subpath contains only one element) it just joins
    /// `dest_dir` with `file_subpath`. For directories (subpath of the form
    /// `dir1/dir2/.../filename`) it does a couple of things:
    ///
    /// * it checks if `dest_dir/dir1` already exists and if we created it
    /// * if it doesn't then it creates the directory
    /// * if it exists and is not created by us it keeps appending (1), (2), ...
    ///   suffix and repeats the prevoius step
    /// * finally appends the rest of subpath components into the final path
    ///  `dest_dir/<mapped dir1>/dir2/../filename`
    ///
    /// The results are cached in RAM to speed this up
    pub fn compose_final_path(
        &mut self,
        dest_dir: &Path,
        file_subpath: &FileSubPath,
    ) -> crate::Result<PathBuf> {
        let mut iter = file_subpath.iter().map(crate::utils::normalize_filename);

        let probe = iter.next().ok_or_else(|| {
            crate::Error::BadPath("Path should contain at least one component".into())
        })?;
        let next = iter.next();

        let mapped = match next {
            Some(next) => {
                // Check if dir exists and is known to us
                let name = match self.mappings.entry(dest_dir.join(probe)) {
                    // Dir is known, reuse
                    Entry::Occupied(occ) => occ.get().clone(),
                    // Dir in new, check if there is name conflict and add to known
                    Entry::Vacant(vacc) => {
                        let mapped = crate::utils::filepath_variants(vacc.key())?.find(|dst_location| {
                                // Skip if there is already a file with the same name.
                                // Additionaly there could be a dangling symlink with the same name,
                                // the `symlink_metadata()` ensures we can catch that.
                                matches!(dst_location.symlink_metadata() , Err(err) if err.kind() == io::ErrorKind::NotFound)
                            })
                            .expect("The filepath variants iterator should never end");

                        let value = vacc.insert(
                            mapped
                                .file_name()
                                .ok_or_else(|| crate::Error::BadPath("Missing file name".into()))?
                                .to_str()
                                .ok_or_else(|| crate::Error::BadPath("Invalid UTF8 path".into()))?
                                .to_string(),
                        );

                        value.clone()
                    }
                };

                [name, next].into_iter().chain(iter).collect()
            }
            None => {
                // Ordinary file
                probe.into()
            }
        };

        Ok(mapped)
    }

    pub fn register_preexisting_final_path(
        &mut self,
        file_subpath: &FileSubPath,
        full_path: impl AsRef<Path>,
    ) {
        self.mappings
            .extend(extract_directory_mapping(file_subpath, full_path.as_ref()));
    }
}

fn extract_directory_mapping(
    file_subpath: &FileSubPath,
    full_path: &Path,
) -> Option<(PathBuf, String)> {
    let mut iter = file_subpath.iter();
    let first = iter.next()?;

    let count = iter.count();

    // Insert only directories
    if count > 0 {
        let ancestor = full_path.ancestors().nth(count)?;

        let filename = ancestor.file_name()?.to_str()?.to_string();

        let path = ancestor.with_file_name(first);
        Some((path, filename))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracting_dir_mapping() {
        let (path, name) = extract_directory_mapping(
            &FileSubPath::from_path("a/b/c.txt").unwrap(),
            "/home/xyz/foo/bar/a(2)/b/c.txt".as_ref(),
        )
        .expect("Failed to read mapping");

        assert_eq!(path, Path::new("/home/xyz/foo/bar/a"));
        assert_eq!(name, "a(2)");
    }
}
