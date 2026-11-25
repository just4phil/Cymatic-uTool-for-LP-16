//! The file system of an export target. The LP-16 reads only FAT32 sticks
//! and rejects exFAT (and NTFS) with a "wrong format" message.

/// Label and file system name (e.g. "FAT32", "exFAT", "NTFS") of the
/// volume whose root is `root` (e.g. "E:\\"). None if Windows cannot tell.
#[cfg(windows)]
pub fn volume_info(root: &str) -> Option<(String, String)> {
    let w = win::wide(root);
    let mut name = [0u16; 128];
    let mut fsn = [0u16; 64];
    let ok = unsafe {
        win::GetVolumeInformationW(
            w.as_ptr(),
            name.as_mut_ptr(),
            name.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fsn.as_mut_ptr(),
            fsn.len() as u32,
        )
    };
    (ok != 0).then(|| (win::from_wide(&name), win::from_wide(&fsn)))
}

#[cfg(not(windows))]
pub fn volume_info(_root: &str) -> Option<(String, String)> {
    None
}

/// File system of the volume that holds `path` (a drive root or any folder
/// on it, e.g. "F:\\Test"). None if the path does not exist or the file
/// system is unknown.
#[cfg(windows)]
pub fn file_system(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let w = win::wide(path);
    let mut root = [0u16; 512];
    let ok = unsafe { win::GetVolumePathNameW(w.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if ok == 0 {
        return None;
    }
    volume_info(&win::from_wide(&root)).map(|(_, fs)| fs).filter(|fs| !fs.is_empty())
}

#[cfg(not(windows))]
pub fn file_system(_path: &str) -> Option<String> {
    None
}

/// Can the LP-16 read a stick with this file system? Only FAT32 is known to
/// work; FAT (FAT16) is untested and counts as not readable.
pub fn lp16_readable(fs: &str) -> bool {
    fs.eq_ignore_ascii_case("FAT32")
}

/// The file system of `path` if it is known and the LP-16 cannot read it,
/// e.g. Some("exFAT"). None = FAT32 or unknown (no warning).
pub fn wrong_file_system(path: &str) -> Option<String> {
    file_system(path).filter(|fs| !lp16_readable(fs))
}

#[cfg(windows)]
#[allow(non_snake_case)]
mod win {
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_comp: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_len: u32,
        ) -> i32;
        pub fn GetVolumePathNameW(path: *const u16, root: *mut u16, len: u32) -> i32;
    }
    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    pub fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fat32_is_readable() {
        assert!(lp16_readable("FAT32"));
        assert!(lp16_readable("fat32"));
        assert!(!lp16_readable("exFAT"));
        assert!(!lp16_readable("NTFS"));
        assert!(!lp16_readable("FAT"));
    }

    #[test]
    fn missing_path_gives_no_warning() {
        assert_eq!(wrong_file_system(""), None);
        assert_eq!(wrong_file_system("Q:\\does\\not\\exist\\lp16"), None);
    }
}
