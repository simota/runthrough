class Runthrough < Formula
  desc "Run GitHub Actions workflows locally"
  homepage "https://github.com/simota/runthrough"
  version "0.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.2.0/runthrough-0.2.0-aarch64-apple-darwin.tar.gz"
      sha256 "37832132e5d493983d258b275097ec51af9bbf1222410db0ee5eaa41a916fe09"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.2.0/runthrough-0.2.0-x86_64-apple-darwin.tar.gz"
      sha256 "6c2d188ba5bfd915d422932a7f2b00cbff5395fbdfb69e6a7ffbd9e99d558bd3"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/simota/runthrough/releases/download/v0.2.0/runthrough-0.2.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "edac993b884d742ddbfb6e605ce3669917055a76f0b21bc113811ea102817726"
    end
    on_intel do
      url "https://github.com/simota/runthrough/releases/download/v0.2.0/runthrough-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "8779006b2e637b3d4e0b6e115d275e55c3f6bef8e8f7053ab4ea009bf29b11d9"
    end
  end

  def install
    bin.install "runthrough"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/runthrough --version")
  end
end
