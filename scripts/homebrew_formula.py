"""Print the Homebrew formula for a release from its archives' checksums.

Usage: python3 -I scripts/homebrew_formula.py <version> <dist dir>

<dist dir> holds runthrough-<version>-<target>.tar.gz.sha256 files as written by
.github/workflows/release.yml (`<sha256>  <file>`). The formula installs the
prebuilt binary for the machine's OS and architecture.
"""
import os
import sys

REPO = "simota/runthrough"
TARGETS = {
    ("macos", "arm"): "aarch64-apple-darwin",
    ("macos", "intel"): "x86_64-apple-darwin",
    ("linux", "arm"): "aarch64-unknown-linux-gnu",
    ("linux", "intel"): "x86_64-unknown-linux-gnu",
}


def sha256(dist, version, target):
    path = os.path.join(dist, f"runthrough-{version}-{target}.tar.gz.sha256")
    with open(path) as f:
        return f.read().split()[0]


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    version, dist = sys.argv[1], sys.argv[2]
    blocks = []
    for os_name in ("macos", "linux"):
        arches = []
        for arch in ("arm", "intel"):
            target = TARGETS[(os_name, arch)]
            archive = f"runthrough-{version}-{target}.tar.gz"
            arches.append(
                f"    on_{arch} do\n"
                f'      url "https://github.com/{REPO}/releases/download/v{version}/{archive}"\n'
                f'      sha256 "{sha256(dist, version, target)}"\n'
                f"    end"
            )
        blocks.append(f"  on_{os_name} do\n" + "\n".join(arches) + "\n  end")
    print(
        f"""class Runthrough < Formula
  desc "Run GitHub Actions workflows locally"
  homepage "https://github.com/{REPO}"
  version "{version}"
  license "MIT"

{chr(10).join(blocks)}

  def install
    bin.install "runthrough"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/runthrough --version")
  end
end""",
    )


if __name__ == "__main__":
    main()
