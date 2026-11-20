# libghostty-vt 1.0.1, vendored

The terminal emulator core waldo links against, as released by the Ghostty
project. The flake builds the library from these files and nothing else, so
the build needs no network and does not depend on where upstream is hosted.

| File | What it is |
|---|---|
| `libghostty-vt-1.0.1.tar.gz` | The release source tarball |
| `libghostty-vt-1.0.1-deps.tar.gz` | The Zig packages the build needs, unpacked by package hash |
| `*.minisig` | Upstream's minisign signatures |
| `SHA256SUMS` | Upstream's checksum list for the whole release; only the two tarballs are kept here |

Downloaded on 2026-11-20 from
`https://release.files.ghostty.org/libghostty-vt/1.0.1/`. All three
signatures verified against upstream's published key.

To check them again:

```sh
cd vendor/libghostty-vt
K=RWQlAjJC23149WL2sEpT/l0QKy7hMIFhYdQOFy0Z7z7PbneUgvlsnYcV
for f in libghostty-vt-1.0.1.tar.gz libghostty-vt-1.0.1-deps.tar.gz SHA256SUMS; do
  minisign -V -P "$K" -m "$f"
done
sha256sum --check --ignore-missing SHA256SUMS
```

To update: download the same six files for the new version, verify them,
replace these, and change `version` in `nix/libghostty-vt.nix`. The source
and dependency tarballs of one version belong together.

The library is MIT licensed; `LICENSE` is at the root of the source tarball.
Licences of the bundled dependencies are in their directories in the
dependency tarball.
