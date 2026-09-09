cask "mzpeak-convert" do
  arch arm: "aarch64", intel: "x86_64"

  version "0.11.3"
  sha256 arm:   "7afa0f3f44fbd70ab249d59455348db7a2146ce7b4c5ab33058416ed25100ee8",
         intel: "fc8c503b7bfee5ed325a30a2819d217ed7d9b15209df7fd9a05f6f9c04ab1ca7"

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

  # The aarch64 binary is built for macOS 11; the x86_64 one runs on older systems
  # than this, but one cask covers both, so the arm64 floor is what we declare.
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

      Converting Thermo .raw needs a .NET 8+ runtime as well; every other input
      format (mzML, imzML, Bruker .d) works with no further dependency:

        brew install --cask dotnet-sdk
    EOS
  end
end
