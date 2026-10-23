//! Names for application-managed directories. User-selected parent paths and
//! files (including OCI/Docker archive members) follow their own formats.
use anyhow::Result;

pub const APP_DIRECTORY: &str = "oci-pull";

pub fn valid_directory_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
}

/// Collision-free, fixed-width encoding: each nibble maps to a..p.
pub fn directory_key(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(b'a' + (byte >> 4)),
                char::from(b'a' + (byte & 15)),
            ]
        })
        .collect()
}

pub fn task_directory_name(id: u64) -> String {
    format!("task-{}", directory_key(&id.to_be_bytes()))
}

pub fn task_id_from_directory_name(name: &str) -> Option<u64> {
    let key = name.strip_prefix("task-")?;
    if key.len() != 16 {
        return None;
    }
    key.bytes().try_fold(0u64, |id, byte| {
        (b'a'..=b'p')
            .contains(&byte)
            .then(|| (id << 4) | u64::from(byte - b'a'))
    })
}

pub fn blob_parts_directory_name(digest: &str) -> Result<String> {
    let bytes = hex::decode(crate::model::digest_hex(digest)?)?;
    Ok(format!("parts-{}", directory_key(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_accept_only_lowercase_ascii_letters_and_hyphens() {
        for name in [
            "oci-pull",
            "cache",
            "tasks",
            "manifests",
            "blobs",
            "locks",
            "previews",
            "cli",
        ] {
            assert!(valid_directory_name(name));
        }
        for name in [
            "", "OCI-Pull", "oci pull", "oci_pull", "task-123", "a.parts", "缓存", "../cache",
            "a/b", "a\\b", "á", "cache\n",
        ] {
            assert!(!valid_directory_name(name), "{name:?}");
        }
    }
    #[test]
    fn task_directory_ids_are_reversible_unique_and_strictly_canonical() {
        let ids = [0, 1, 15, 16, 42, 1792723503122, u64::MAX];
        let mut names = std::collections::HashSet::new();
        for id in ids {
            let name = task_directory_name(id);
            assert!(valid_directory_name(&name));
            assert_eq!(task_id_from_directory_name(&name), Some(id));
            assert!(names.insert(name));
        }
        for name in [
            "42",
            "task-a",
            "task-aaaaaaaaaaaaaaa0",
            "task-aaaaaaaaaaaaaaaq",
            "TASK-aaaaaaaaaaaaaaaa",
            "task-aaaaaaaaaaaaaaaaa",
        ] {
            assert!(task_id_from_directory_name(name).is_none());
        }
    }
    #[test]
    fn binary_keys_and_chunk_directories_preserve_every_digest_bit() {
        let all_bytes: Vec<u8> = (0..=255).collect();
        let encoded = directory_key(&all_bytes);
        assert!(valid_directory_name(&encoded));
        assert_eq!(encoded.len(), 512);
        let decoded: Vec<u8> = encoded
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| ((pair[0] - b'a') << 4) | (pair[1] - b'a'))
            .collect();
        assert_eq!(decoded, all_bytes);
        let digest = format!("sha256:{}", "0123456789abcdef".repeat(4));
        let name = blob_parts_directory_name(&digest).unwrap();
        assert_eq!(name, format!("parts-{}", "abcdefghijklmnop".repeat(4)));
        assert!(valid_directory_name(&name));
        assert!(blob_parts_directory_name("../../escape").is_err());
    }
}
