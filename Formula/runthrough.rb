class Runthrough < Formula
  desc "Run GitHub Actions workflows locally"
  homepage "https://github.com/simota/runthrough"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.1.0/runthrough-0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "b2187eb296d51683b28e0583b1489a41ec699bf45eacc12342dec794ca5c7aca"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.1.0/runthrough-0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "82c298140961993f16de6a5119c69b8588d776f5dc04509a1b5a3301bb870148"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.1.0/runthrough-0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "d4a4b696471e966d8133766ed7bef647e693acfa036650ce916fb554f7d99220"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.1.0/runthrough-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "00da03884b76a55aefb22eb498543fa4252993c5d778b989a990a0a3b07baad3"
    end
  end

  def install
    bin.install "runthrough"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/runthrough --version")
  end
end
