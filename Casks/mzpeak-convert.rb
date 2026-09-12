cask "mzpeak-convert" do
  arch arm: "aarch64", intel: "x86_64"

  version "0.12.2"
  sha256 arm:   "a63598bb07bc1467380c4c56e9ee35d9a01a09cae888208eb2bab66320519410",
         intel: "5237c6579536f65fc6439ee4c08d6f85b4a8862c40acb8146e67843e41b18e5b"

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

  # No quarantine stanza since 0.12.2. Through 0.12.1 the binaries were ad-hoc signed,
  # and macOS killed a quarantined binary carrying no Developer ID the moment it was
  # executed ("killed: 9", measured 2026-09-09 on macOS 26.5) — so this cask stripped
  # com.apple.quarantine from the one file it installs. They are now signed with a
  # Developer ID certificate and notarized, which is what that attribute exists to
  # test, so there is nothing left to strip. release.yml refuses to update this tap
  # from a release whose macOS binaries are not signed, because that state plus this
  # file would install a tool macOS kills, with every check green.

  caveats do
    <<~EOS
      mzpeak-convert is signed with a Developer ID certificate and notarized by Apple.
      A .tar.gz cannot carry a stapled ticket — no archive format can — so the first
      run after installation checks with Apple over the network; every run after that
      is offline. On a machine with no network at install time, that first run may
      pause briefly.

      Homebrew checked the downloaded archive against the digest in this cask, which
      is the one published as a .sha256 file beside the archive on the releases page.
      To look at the signature yourself:

        codesign -dvv "$(brew --prefix)/bin/mzpeak-convert"
        spctl -a -vv "$(brew --prefix)/bin/mzpeak-convert"

      On macOS this reads mzML, imzML and Bruker .d (TDF/TSF); Thermo .raw needs a
      .NET 8+ runtime as well. The Bruker BAF, Waters, SciEX, Agilent and Shimadzu
      lanes are Windows-only — route those through msconvert. See
      docs/PLATFORM_SUPPORT.md.

        brew install --cask dotnet-sdk

    EOS
  end
end
