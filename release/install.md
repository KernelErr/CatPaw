
## Install

macOS (Apple silicon) and Linux (x86_64, aarch64):

    curl -fsSL https://catpaw.sh/install.sh | sh

Windows (PowerShell):

    irm https://catpaw.sh/install.ps1 | iex

The scripts download the archive for your system from this release and
check it against `SHA256SUMS`. Or download an archive below, check it
(`sha256sum -c SHA256SUMS --ignore-missing`), and put `catpaw` on your
PATH. Then connect it to your agent:

    catpaw setup claude-code

Intel Macs and other systems build from source: see the README.
