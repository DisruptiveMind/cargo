use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::hash::{Hash, Hasher, SipHasher};
use std::io::{self, fs, File, BufferedReader};
use std::io::fs::PathExtensions;

use core::{Package, Target};
use util;
use util::{CargoResult, Fresh, Dirty, Freshness, internal, profile, ChainError};

use super::Kind;
use super::job::Work;
use super::context::Context;

/// A tuple result of the `prepare_foo` functions in this module.
///
/// The first element of the triple is whether the target in question is
/// currently fresh or not, and the second two elements are work to perform when
/// the target is dirty or fresh, respectively.
///
/// Both units of work are always generated because a fresh package may still be
/// rebuilt if some upstream dependency changes.
pub type Preparation = (Freshness, Work, Work);

/// Prepare the necessary work for the fingerprint for a specific target.
///
/// When dealing with fingerprints, cargo gets to choose what granularity
/// "freshness" is considered at. One option is considering freshness at the
/// package level. This means that if anything in a package changes, the entire
/// package is rebuilt, unconditionally. This simplicity comes at a cost,
/// however, in that test-only changes will cause libraries to be rebuilt, which
/// is quite unfortunate!
///
/// The cost was deemed high enough that fingerprints are now calculated at the
/// layer of a target rather than a package. Each target can then be kept track
/// of separately and only rebuilt as necessary. This requires cargo to
/// understand what the inputs are to a target, so we drive rustc with the
/// --dep-info flag to learn about all input files to a unit of compilation.
///
/// This function will calculate the fingerprint for a target and prepare the
/// work necessary to either write the fingerprint or copy over all fresh files
/// from the old directories to their new locations.
pub fn prepare_target(cx: &mut Context, pkg: &Package, target: &Target,
                      kind: Kind) -> CargoResult<Preparation> {
    let _p = profile::start(format!("fingerprint: {} / {:?}",
                                    pkg.get_package_id(), target));
    let new = dir(cx, pkg, kind);
    let loc = new.join(filename(target));
    cx.layout(pkg, kind).proxy().whitelist(&loc);

    // We want to use the package fingerprint if we're either a doc target or a
    // path source. If we're a git/registry source, then the mtime of files may
    // fluctuate, but they won't change so long as the source itself remains
    // constant (which is the responsibility of the source)
    let use_pkg = {
        let doc = target.get_profile().is_doc();
        let path = pkg.get_summary().get_source_id().is_path();
        doc || !path
    };

    info!("fingerprint at: {}", loc.display());

    // First bit of the freshness calculation, whether the dep-info file
    // indicates that the target is fresh.
    let dep_info = dep_info_loc(cx, pkg, target, kind);
    let mut are_files_fresh = use_pkg ||
                              try!(calculate_target_fresh(&dep_info));

    // Second bit of the freshness calculation, whether rustc itself, the
    // target are fresh, and the enabled set of features are all fresh.
    let features = cx.resolve.features(pkg.get_package_id());
    let features = features.map(|s| {
        let mut v = s.iter().collect::<Vec<&String>>();
        v.sort();
        v
    });
    let rustc_fingerprint = if use_pkg {
        mk_fingerprint(cx, &(target, try!(calculate_pkg_fingerprint(cx, pkg)),
                             features))
    } else {
        mk_fingerprint(cx, &(target, features))
    };
    let is_rustc_fresh = try!(is_fresh(&loc, rustc_fingerprint.as_slice()));

    let root = cx.out_dir(pkg, kind, target);
    if !target.get_profile().is_doc() {
        for filename in try!(cx.target_filenames(target)).iter() {
            let dst = root.join(filename);
            cx.layout(pkg, kind).proxy().whitelist(&dst);
            if are_files_fresh && !dst.exists() {
                are_files_fresh = false;
            }

            if target.get_profile().is_test() {
                cx.compilation.tests.push((target.get_name().to_string(), dst));
            } else if target.is_bin() {
                cx.compilation.binaries.push(dst);
            } else if target.is_lib() {
                let pkgid = pkg.get_package_id().clone();
                match cx.compilation.libraries.entry(pkgid) {
                    Occupied(entry) => entry.into_mut(),
                    Vacant(entry) => entry.insert(Vec::new()),
                }.push(dst);
            }
        }
    }

    Ok(prepare(is_rustc_fresh && are_files_fresh, loc, rustc_fingerprint))
}

/// Prepare the necessary work for the fingerprint of a build command.
///
/// Build commands are located on packages, not on targets. Additionally, we
/// don't have --dep-info to drive calculation of the fingerprint of a build
/// command. This brings up an interesting predicament which gives us a few
/// options to figure out whether a build command is dirty or not:
///
/// 1. A build command is dirty if *any* file in a package changes. In theory
///    all files are candidate for being used by the build command.
/// 2. A build command is dirty if any file in a *specific directory* changes.
///    This may lose information as it may require files outside of the specific
///    directory.
/// 3. A build command must itself provide a dep-info-like file stating how it
///    should be considered dirty or not.
///
/// The currently implemented solution is option (1), although it is planned to
/// migrate to option (2) in the near future.
pub fn prepare_build_cmd(cx: &mut Context, pkg: &Package,
                         target: Option<&Target>) -> CargoResult<Preparation> {
    let _p = profile::start(format!("fingerprint build cmd: {}",
                                    pkg.get_package_id()));

    // TODO: this should not explicitly pass Kind::Target
    let kind = Kind::Target;

    if pkg.get_manifest().get_build().len() == 0 && target.is_none() {
        return Ok((Fresh, Work::noop(), Work::noop()));
    }
    let new = dir(cx, pkg, kind);
    let loc = new.join("build");
    cx.layout(pkg, kind).proxy().whitelist(&loc);

    info!("fingerprint at: {}", loc.display());

    let new_fingerprint = try!(calculate_build_cmd_fingerprint(cx, pkg));
    let new_fingerprint = mk_fingerprint(cx, &new_fingerprint);

    let is_fresh = try!(is_fresh(&loc, new_fingerprint.as_slice()));

    // The new custom build command infrastructure handles its own output
    // directory as part of freshness.
    if target.is_none() {
        let native_dir = cx.layout(pkg, kind).native(pkg);
        cx.compilation.native_dirs.insert(pkg.get_package_id().clone(),
                                          native_dir);
    }

    Ok(prepare(is_fresh, loc, new_fingerprint))
}

/// Prepare work for when a package starts to build
pub fn prepare_init(cx: &mut Context, pkg: &Package, kind: Kind)
                    -> (Work, Work) {
    let new1 = dir(cx, pkg, kind);
    let new2 = new1.clone();

    let work1 = Work::new(move |_| {
        if !new1.exists() {
            try!(fs::mkdir(&new1, io::USER_DIR));
        }
        Ok(())
    });
    let work2 = Work::new(move |_| {
        if !new2.exists() {
            try!(fs::mkdir(&new2, io::USER_DIR));
        }
        Ok(())
    });

    (work1, work2)
}

/// Given the data to build and write a fingerprint, generate some Work
/// instances to actually perform the necessary work.
fn prepare(is_fresh: bool, loc: Path, fingerprint: String) -> Preparation {
    let write_fingerprint = Work::new(move |desc_tx| {
        drop(desc_tx);
        try!(File::create(&loc).write_str(fingerprint.as_slice()));
        Ok(())
    });

    (if is_fresh {Fresh} else {Dirty}, write_fingerprint, Work::noop())
}

/// Return the (old, new) location for fingerprints for a package
pub fn dir(cx: &Context, pkg: &Package, kind: Kind) -> Path {
    cx.layout(pkg, kind).proxy().fingerprint(pkg)
}

/// Returns the (old, new) location for the dep info file of a target.
pub fn dep_info_loc(cx: &Context, pkg: &Package, target: &Target,
                    kind: Kind) -> Path {
    let ret = dir(cx, pkg, kind).join(format!("dep-{}", filename(target)));
    cx.layout(pkg, kind).proxy().whitelist(&ret);
    return ret;
}

fn is_fresh(loc: &Path, new_fingerprint: &str) -> CargoResult<bool> {
    let mut file = match File::open(loc) {
        Ok(file) => file,
        Err(..) => return Ok(false),
    };

    let old_fingerprint = try!(file.read_to_string());

    log!(5, "old fingerprint: {}", old_fingerprint);
    log!(5, "new fingerprint: {}", new_fingerprint);

    Ok(old_fingerprint.as_slice() == new_fingerprint)
}

/// Frob in the necessary data from the context to generate the real
/// fingerprint.
fn mk_fingerprint<T: Hash<SipHasher>>(cx: &Context, data: &T) -> String {
    let mut hasher = SipHasher::new_with_keys(0,0);
    (cx.config.rustc_version(), data).hash(&mut hasher);
    util::to_hex(hasher.finish())
}

fn calculate_target_fresh(dep_info: &Path) -> CargoResult<bool> {
    macro_rules! fs_try {
        ($e:expr) => (match $e { Ok(e) => e, Err(..) => return Ok(false) })
    }
    let mut f = BufferedReader::new(fs_try!(File::open(dep_info)));
    // see comments in append_current_dir for where this cwd is manifested from.
    let cwd = fs_try!(f.read_until(0));
    let cwd = Path::new(&cwd[..cwd.len()-1]);
    let line = match f.lines().next() {
        Some(Ok(line)) => line,
        _ => return Ok(false),
    };
    let line = line.as_slice();
    let mtime = try!(fs::stat(dep_info)).modified;
    let pos = try!(line.find_str(": ").chain_error(|| {
        internal(format!("dep-info not in an understood format: {}",
                         dep_info.display()))
    }));
    let deps = line.slice_from(pos + 2);

    let mut deps = deps.split(' ').map(|s| s.trim()).filter(|s| !s.is_empty());
    loop {
        let mut file = match deps.next() {
            Some(s) => s.to_string(),
            None => break,
        };
        while file.as_slice().ends_with("\\") {
            file.pop();
            file.push(' ');
            file.push_str(deps.next().unwrap())
        }
        match fs::stat(&cwd.join(file.as_slice())) {
            Ok(stat) if stat.modified <= mtime => {}
            Ok(stat) => {
                info!("stale: {} -- {} vs {}", file, stat.modified, mtime);
                return Ok(false)
            }
            _ => { info!("stale: {} -- missing", file); return Ok(false) }
        }
    }

    Ok(true)
}

fn calculate_build_cmd_fingerprint(cx: &Context, pkg: &Package)
                                   -> CargoResult<String> {
    // TODO: this should be scoped to just the `build` directory, not the entire
    // package.
    calculate_pkg_fingerprint(cx, pkg)
}

fn calculate_pkg_fingerprint(cx: &Context, pkg: &Package) -> CargoResult<String> {
    let source = cx.sources
        .get(pkg.get_package_id().get_source_id())
        .expect("BUG: Missing package source");

    source.fingerprint(pkg)
}

fn filename(target: &Target) -> String {
    let kind = if target.is_lib() {"lib"} else {"bin"};
    let flavor = if target.get_profile().is_test() {
        "test-"
    } else if target.get_profile().is_doc() {
        "doc-"
    } else {
        ""
    };
    format!("{}{}-{}", flavor, kind, target.get_name())
}

// The dep-info files emitted by the compiler all have their listed paths
// relative to whatever the current directory was at the time that the compiler
// was invoked. As the current directory may change over time, we need to record
// what that directory was at the beginning of the file so we can know about it
// next time.
pub fn append_current_dir(path: &Path, cwd: &Path) -> CargoResult<()> {
    let mut f = try!(File::open_mode(path, io::Open, io::ReadWrite));
    let contents = try!(f.read_to_end());
    try!(f.seek(0, io::SeekSet));
    try!(f.write(cwd.as_vec()));
    try!(f.write(&[0]));
    try!(f.write(&contents[]));
    Ok(())
}
