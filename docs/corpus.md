# Test corpus

Moraine is benchmarked and validated against a snapshot of real Gentoo data so
its results and timings can be compared directly with stock Portage. That
snapshot lives in the git-ignored `corpus/` directory at the repository root.

The corpus is never committed. Each contributor populates it from a real Gentoo
system (or a stage tarball).

## Layout

Place the data under `corpus/` mirroring the system paths it came from:

```
corpus/
  repos/<repo>/metadata/md5-cache/   # ebuild metadata cache (per-repo)
  repos/<repo>/profiles/             # profiles, including profiles/repo_name
  repos/<repo>/metadata/layout.conf  # repository layout
  db/pkg/                            # installed package database (vdb)
  etc/portage/                       # make.conf, repos.conf, package.* files
  etc/portage/make.profile           # the active profile symlink target
```

## Populating from a running Gentoo system

Copy the relevant trees from the host. Adjust `<repo>` and paths to match the
system being captured:

```sh
mkdir -p corpus/repos corpus/db corpus/etc

# Repository (metadata cache, profiles, layout). The md5-cache is what the
# resolver reads, so it is the important part.
rsync -a --delete /var/db/repos/gentoo/ corpus/repos/gentoo/

# Installed package database (vdb).
rsync -a --delete /var/db/pkg/ corpus/db/pkg/

# Configuration.
rsync -a --delete /etc/portage/ corpus/etc/portage/
```

A metadata-only corpus (just `repos/<repo>/metadata/md5-cache`, the profiles,
and `etc/portage`) is enough to exercise dependency resolution. Add `db/pkg` to
exercise update and slot-operator-rebuild paths.

## Using it

The corpus harness entry point validates and summarizes a corpus directory.
During bootstrap it only counts top-level entries; the metadata and
installed-store importers replace that body in later phases. Point the harness
at `corpus/` (or any captured path) to import it.
