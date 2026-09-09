cask "mzpeak-convert" do
  arch arm: "aarch64", intel: "x86_64"

  version "0.11.4"
  sha256 arm:   "5cc2b92613fe1e74d8a61955f977d09e5d9b0f850ac4bccb60756b2e98d8c9e5",
         intel: "703a904346aee96e9371b173b296a64e7437453424201c3af9fbe93332bb35e5"

  url "https://github.com/okohlbacher/mzPeakConverter/releases/download/v#{version}/" \
      "mzpeak-convert-#{version}-#{arch}-apple-darwin.tar.gz"
  name "mzpeak-convert"
  name "mzPeakConverter"
  desc "Converter from mass-spectrometry formats to the HUPO-PSI mzPeak format"
  homepage "https://github.com/okohlbacher/mzPeakConverter"

  livecheck do
    url :url
    strategy :github_latest
  end

  # Both archives are built with MACOSX_DEPLOYMENT_TARGET=11.0 (release.yml), so Big
  # Sur is the real floor for either architecture.
  depends_on macos: :big_sur

  binary "mzpeak-convert"

  # The released binaries are ad-hoc signed, not notarized by Apple. Homebrew marks
  # every cask download with com.apple.quarantine, and macOS kills a quarantined
  # binary that carries no Developer ID the moment it is executed ("killed: 9",
  # measured 2026-09-09 on macOS 26.5). Removing the attribute from the one file
  # this cask installs is what makes it runnable; the caveats say so out loud.
  # Delete this stanza once the release workflow signs and notarizes the binaries.
  postflight_steps do
    run "/usr/bin/xattr",
        args:           ["-dr", "com.apple.quarantine", "mzpeak-convert"],
        chdir:          ".",
        writable_paths: ["mzpeak-convert"]
  end

  caveats do
    <<~EOS
      mzpeak-convert ships as an ad-hoc signed binary, not notarized by Apple, so
      this cask removes the download-quarantine attribute from the executable it
      installs. Without that, macOS kills the tool the first time it runs.

      Homebrew checked the downloaded archive against the digest in this cask, which
      is the one published as a .sha256 file beside the archive on the releases page.
      To look at the signature yourself:

        codesign -dvv "$(brew --prefix)/bin/mzpeak-convert"

      On macOS this reads mzML, imzML and Bruker .d (TDF/TSF); Thermo .raw needs a
      .NET 8+ runtime as well. The Bruker BAF, Waters, SciEX, Agilent and Shimadzu
      lanes are Windows-only — route those through msconvert. See
      docs/PLATFORM_SUPPORT.md.

        brew install --cask dotnet-sdk

      The formula in the same tap installs the same binary without this cask's
      quarantine step. Install one of the two, not both: they provide the same
      command, and uninstalling either would take it off your PATH.
    EOS
  end
end
