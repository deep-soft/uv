# PEP440 in rust

[![Crates.io](https://img.shields.io/crates/v/pep440_rs.svg?logo=rust&style=flat-square)](https://crates.io/crates/pep440_rs)
[![PyPI](https://img.shields.io/pypi/v/pep440_rs.svg?logo=python&style=flat-square)](https://pypi.org/project/pep440_rs)

A library for python version numbers and specifiers, implementing
[PEP 440](https://peps.python.org/pep-0440). See
[Reimplementing PEP 440](https://cohost.org/konstin/post/514863-reimplementing-pep-4) for some
background.

Higher level bindings to the requirements syntax are available in
[pep508_rs](https://github.com/konstin/pep508_rs).

```rust
use std::str::FromStr;
use pep440_rs::{parse_version_specifiers, Version, VersionSpecifier};

let version = Version::from_str("1.19").unwrap();
let version_specifier = VersionSpecifier::from_str("==1.*").unwrap();
assert!(version_specifier.contains(&version));
let version_specifiers = parse_version_specifiers(">=1.16, <2.0").unwrap();
assert!(version_specifiers.contains(&version));
```

PEP 440 has a lot of unintuitive features, including:

- An epoch that you can prefix the version with, e.g., `1!1.2.3`. Lower epoch always means lower
  version (`1.0 <=2!0.1`)
- Post versions, which can be attached to both stable releases and pre-releases
- Dev versions, which can be attached to both stable releases and pre-releases. When attached to a
  pre-release the dev version is ordered just below the normal pre-release, however when attached to
  a stable version, the dev version is sorted before a pre-releases
- Pre-release handling is a mess: "Pre-releases of any kind, including developmental releases, are
  implicitly excluded from all version specifiers, unless they are already present on the system,
  explicitly requested by the user, or if the only available version that satisfies the version
  specifier is a pre-release.". This means that we can't say whether a specifier matches without
  also looking at the environment
- Pre-release vs. pre-release incl. dev is fuzzy
- Local versions on top of all the others, which are added with a + and have implicitly typed string
  and number segments
- No semver-caret (`^`), but a pseudo-semver tilde (`~=`)
- Ordering contradicts matching: We have, e.g., `1.0+local > 1.0` when sorting, but `==1.0` matches
  `1.0+local`. While the ordering of versions itself is a total order the version matching needs to
  catch all sorts of special cases
