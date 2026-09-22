cask "mzpeak-convert" do
  arch arm: "aarch64", intel: "x86_64"

  version "0.13.0"
  sha256 arm:   "5831df56071ea834bdee306bce3de56c9e909daaf370b3c8036eb9d984e8c2c2",
         intel: "b6046db5d5415454980aec02fae9e39e371a4cf92350916e1f4522a907749ea7"

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
