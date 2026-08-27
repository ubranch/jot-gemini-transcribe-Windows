# winget manifests

Ready to submit, not yet submitted. `winget` installs from Microsoft's
[winget-pkgs](https://github.com/microsoft/winget-pkgs) repository, so these
files only do anything once they land there as a pull request.

## Submitting a version

1. Build and publish the release first: the manifest points at a download URL
   that has to already exist.
2. Update `PackageVersion` in all three files under `manifests/`, and `InstallerUrl`,
   `InstallerSha256` and `ReleaseDate` in the installer manifest.
   `scripts/package.ps1` prints the SHA256 of the zip it builds.
3. Check them locally, which catches schema mistakes before a reviewer does:

   ```powershell
   winget validate --manifest packaging\winget\manifests
   ```

4. Open a pull request against `microsoft/winget-pkgs`, copying everything in
   `manifests/` to `manifests/u/ubranch/Jot/<version>/` there.

## Why `zip` and not `portable`

Jot ships as a zip of the executable and the licence files it is required to
ship with. `InstallerType: zip` with `NestedInstallerType: portable` tells
winget to unpack it and put `jot.exe` on the PATH, which is what a portable app
without an installer should do.

The binary is unsigned, so expect a reviewer to say so. That is accurate: see
the note in `scripts/package.ps1` about what signing would take.
