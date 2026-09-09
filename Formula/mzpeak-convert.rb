class MzpeakConvert < Formula
  desc "Converter from mass-spectrometry formats to the HUPO-PSI mzPeak format"
  homepage "https://github.com/okohlbacher/mzPeakConverter"
  # Load-bearing, not redundant with the URL: the URLs below are built from it.
  version "0.11.4"
  license "MIT"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on macos: :big_sur

  # The released archive for this machine, not a source build: the dependency tree
  # (arrow, parquet, mzdata) takes minutes to compile and the binary is self-contained.
  on_macos do
    on_arm do
      url "https://github.com/okohlbacher/mzPeakConverter/releases/download/v#{version}/" \
          "mzpeak-convert-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "5cc2b92613fe1e74d8a61955f977d09e5d9b0f850ac4bccb60756b2e98d8c9e5"
    end
    on_intel do
      url "https://github.com/okohlbacher/mzPeakConverter/releases/download/v#{version}/" \
          "mzpeak-convert-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "703a904346aee96e9371b173b296a64e7437453424201c3af9fbe93332bb35e5"
    end
  end

  def install
    bin.install "mzpeak-convert"
    doc.install "README.md"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/mzpeak-convert --version")
  end
end
