//! Cross-impl trait test. Exercises the [`Vfs`] surface so each impl
//! gets the same behavioural contract — invoked from each impl's tests
//! module with the impl's preferred root path (tempdir for RealVfs,
//! "/" for TempVfs/MemVfs).

use std::path::Path;

use crate::vfs::Vfs;

/// Run read+write+walk against `vfs`, using `base` as the directory all
/// paths are created under. `base` must be a directory that already
/// exists *or* that the impl will happily create — both RealVfs (tempdir)
/// and TempVfs (root-rebased) and MemVfs (auto-creates ancestors)
/// satisfy that.
pub fn trait_roundtrip(vfs: &dyn Vfs, base: &Path) {
    let f1 = base.join("rt/a.txt");
    let f2 = base.join("rt/sub/b.txt");

    vfs.write(&f1, b"alpha").expect("write f1");
    vfs.write(&f2, b"beta").expect("write f2");

    assert_eq!(vfs.read(&f1).unwrap(), b"alpha");
    assert_eq!(vfs.read(&f2).unwrap(), b"beta");

    let mut walked = vfs.walk(&base.join("rt")).unwrap();
    walked.sort();
    assert_eq!(walked.len(), 2, "walk should find both files, got: {walked:?}");

    // Metadata sanity
    assert!(vfs.metadata(&f1).unwrap().is_file);

    // remove_all cleans up
    vfs.remove_all(&base.join("rt")).unwrap();
    assert!(!vfs.exists(&f1));
    assert!(!vfs.exists(&f2));
}
