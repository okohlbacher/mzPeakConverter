class MzpeakConvert < Formula
  desc "Converter from mass-spectrometry formats to the HUPO-PSI mzPeak format"
  homepage "https://github.com/okohlbacher/mzPeakConverter"
  # Load-bearing, not redundant with the URL: the URLs below are built from it.
  version "0.11.3"
  license "MIT"

  # The released archive for this machine, not a source build: the dependency tree
  # (arrow, parquet, mzdata) takes minutes to compile and the binary is self-contained.
  on_macos do
    on_arm do
      url "https://github.com/okohlbacher/mzPeakConverter/releases/download/v#{version}/" \
          "mzpeak-convert-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "7afa0f3f44fbd70ab249d59455348db7a2146ce7b4c5ab33058416ed25100ee8"
    end
    on_intel do
      url "https://github.com/okohlbacher/mzPeakConverter/releases/download/v#{version}/" \
          "mzpeak-convert-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "fc8c503b7bfee5ed325a30a2819d217ed7d9b15209df7fd9a05f6f9c04ab1ca7"
    end
  end

  livecheck do
    url "https://github.com/okohlbacher/mzPeakConverter/releases/latest"
    strategy :github_latest
  end

  depends_on macos: :big_sur

  def install
    bin.install "mzpeak-convert"
    doc.install "README.md"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/mzpeak-convert --version")
  end
end
