class Runthrough < Formula
  desc "Run GitHub Actions workflows locally"
  homepage "https://github.com/simota/runthrough"
  version "0.2.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.2.1/runthrough-0.2.1-aarch64-apple-darwin.tar.gz"
      sha256 "ea6309dfd679605941091e16658345f5badd9c7cb3f3600bcdbadf520196878d"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.2.1/runthrough-0.2.1-x86_64-apple-darwin.tar.gz"
      sha256 "33ee0d3ff6ee701cf45f709305590dcfe4eacc258c565cbdfb456319a87bd076"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.2.1/runthrough-0.2.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "c383efe0cc504950ad9124f57b25c5334754862475d59bb592183e56fa713d9d"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.2.1/runthrough-0.2.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "40e84de527ff6a7dc6fadf7bc350c0ca101df905bb80f53ff4d30388878ac6f0"
    end
  end

  def install
    bin.install "runthrough"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/runthrough --version")
  end
end
