class Runthrough < Formula
  desc "Run GitHub Actions workflows locally"
  homepage "https://github.com/simota/runthrough"
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.3.0/runthrough-0.3.0-aarch64-apple-darwin.tar.gz"
      sha256 "78b2fe640a0e46aa1f986475d09267fa1653bc1640c32799c1677214a0fef28d"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.3.0/runthrough-0.3.0-x86_64-apple-darwin.tar.gz"
      sha256 "5e635aefa14e59ab85114a2c42e8e8c1b6e798a6762e97f2dba479848a99b42d"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.3.0/runthrough-0.3.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "ca0b0ab3dc5aeebbc228742fb0d25b21d322a46f917a8c6f4f3af8e8baf5d97b"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.3.0/runthrough-0.3.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "d081d74a5978d973e998898428b836c05f727e96bebc647450900a72f18c5560"
    end
  end

  def install
    bin.install "runthrough"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/runthrough --version")
  end
end
