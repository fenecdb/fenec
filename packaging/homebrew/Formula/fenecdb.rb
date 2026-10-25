# Written by tools/homebrew-formula.py in fenecdb/fenecdb from release v0.1.11's
# SHA256SUMS: the release workflow rewrites it, so a change by hand is lost.
class Fenecdb < Formula
  desc "Embedded database with its own query language, FenecQL"
  homepage "https://fenecdb.com"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    on_arm do
      url "https://github.com/fenecdb/fenecdb/releases/download/v0.1.11/fenec-v0.1.11-aarch64-apple-darwin.tar.gz"
      sha256 "e2f89f0e6e6a6728a17065cfe02a447bbc260eb6c657caf57754fda7c0d2ea51"
    end
    on_intel do
      url "https://github.com/fenecdb/fenecdb/releases/download/v0.1.11/fenec-v0.1.11-x86_64-apple-darwin.tar.gz"
      sha256 "48a610c61456cfd6ef0c5e43723f169d343699b0fd6cc1b00e62dab12c332025"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/fenecdb/fenecdb/releases/download/v0.1.11/fenec-v0.1.11-aarch64-unknown-linux-musl.tar.gz"
      sha256 "fa8e492b5be0f7f2d0f1ba19c3d6cc733c1e66177f76d44221030b570f2f05c1"
    end
    on_intel do
      url "https://github.com/fenecdb/fenecdb/releases/download/v0.1.11/fenec-v0.1.11-x86_64-unknown-linux-musl.tar.gz"
      sha256 "d5acff57f2b960051e00c66a872080580bfb7ff3f3cb49ae5e21fa8fddf38d8d"
    end
  end

  def install
    bin.install "fenec", "fenec-server"
  end

  test do
    # Neither binary has a version flag: fenec-server names its version on the
    # first line of --help, and the shell is run on a script, in memory.
    assert_match "fenec-server #{version}", shell_output("#{bin}/fenec-server --help 2>&1")
    script = "create collection t (n int)\nput t {id: 1, n: 7}\nget t\n"
    assert_match(/^1\s+7$/, pipe_output("#{bin}/fenec", script))
  end
end
