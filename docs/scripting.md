# Using CatPaw from scripts

An agent that has read a site with CatPaw often wants to repeat the job
without itself in the loop: a script that fetches the same pages every
day, a scraper, a check in CI. There are two ways to drive CatPaw from a
script, and neither needs an SDK.

- **`catpaw fetch`**, one command per page: load it, optionally take a
  few actions, print what you ask for, exit. Good for one page at a time.
- **`catpaw mcp --stdio`**, one long-running browser: the script sends
  the same tool calls an agent makes (navigate, click, fill, read,
  evaluate) as JSON lines and reads the answers. Good for many steps in
  one session.

## One page, one answer

`--js` runs the page's scripts first; `--eval` then evaluates an
expression in the page, awaits it if it is a promise, and prints the
result on stdout. Everything else (the request line, the script summary,
`--console` output) goes to stderr, so stdout is just the answer:

```sh
catpaw fetch https://example.com --js \
    --eval "JSON.stringify({title: document.title, links: [...document.links].map(a => a.href)})"
# {"title":"Example Domain","links":["https://iana.org/help/example-domains"]}
```

Return a string; `JSON.stringify` is the easy way to return structure.
The exit code is non-zero when the page could not be loaded or the
expression threw.

Without `--eval`, `--view` picks what to print: `snapshot` (the default),
`markdown`, `text`, `html`, `links` or `forms`. These work without `--js`
too, for pages that do not need their scripts.

### Act first, then read

`--action` takes a step once the page has settled, and may repeat; the
steps run in order before the output:

```sh
catpaw fetch https://site.example/search --js \
    --action "fill input[name=q] rust browser" --action "press Enter" \
    --eval "JSON.stringify([...document.querySelectorAll('h3')].map(h => h.textContent))"
```

The steps are `click`, `fill`, `type`, `press`, `check`, `uncheck`,
`select`, `hover`, `focus`, `frame` (to act inside an iframe), `back` and
`forward`; `catpaw fetch --help` has their arguments.

### Keep a session between runs

`--cookie-jar jar.json` loads cookies before the request and saves them
after it; `--storage storage.json` does the same for `localStorage`. Log
in once, and later runs are logged in:

```sh
catpaw fetch https://site.example/login --js --cookie-jar jar.json \
    --action "fill #username bob" --action "fill #password secret" --action "press Enter" --text
catpaw fetch https://site.example/account --js --cookie-jar jar.json --text
```

Keep such files private: they hold the session.

## Many steps in one session

`catpaw mcp --stdio` reads JSON-RPC requests, one per line, on stdin and
answers each on stdout. After the `initialize` handshake, every
`tools/call` is one step, with the same tools and arguments an agent
uses; the answer is text that starts with `ok <tool>` or `error <tool>`.
Pass `"snapshot": "none"` to actions when the script does not need the
page's outline after each one. `evaluate` answers `ok evaluate` and, on
the next line, the value.

A complete client in Python, with nothing beyond the standard library:

```python
import json
import subprocess


class CatPaw:
    def __init__(self, *args):
        self.proc = subprocess.Popen(
            ["catpaw", "mcp", "--stdio", *args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        self.next_id = 0
        self.request("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "my-script", "version": "1"},
        })
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while True:
            reply = json.loads(self.proc.stdout.readline())
            if reply.get("id") == self.next_id:
                return reply

    def call(self, tool, **arguments):
        reply = self.request("tools/call", {"name": tool, "arguments": arguments})
        text = reply["result"]["content"][0]["text"]
        if reply["result"].get("isError"):
            raise RuntimeError(text)
        return text

    def evaluate(self, script):
        return self.call("evaluate", script=script).split("\n", 1)[1]

    def close(self):
        self.proc.stdin.close()
        self.proc.wait()


browser = CatPaw()
browser.call("navigate", url="https://books.toscrape.com/", snapshot="none")
books = []
for _ in range(3):
    books += json.loads(browser.evaluate(
        "JSON.stringify([...document.querySelectorAll('article.product_pod')].map(a => ({"
        "title: a.querySelector('h3 a').title,"
        "price: a.querySelector('.price_color').textContent})))"
    ))
    browser.call("click", target='link "next"', snapshot="none")
print(len(books), books[0])
browser.close()
```

Targets are what an agent uses: a ref from a snapshot (`e12`), a role and
name (`link "next"`), visible text, or a CSS selector; the tool list
(`tools/list`) describes every argument. Any language that can start a
process and read lines can do the same.

## Approvals when nobody is watching

By default, sending a form and uploading a file wait for the user's
approval: CatPaw opens an approval page in their browser and the call
waits for the answer. A script that runs unattended would wait in vain,
so tell CatPaw up front what it may do:

- `--trust site.example` lets submissions to that host (and its
  subdomains) go without approval. Use it for sites you run or whose
  forms the script is meant to send.
- `--policy open` asks for nothing at all. Use it for test runs against
  your own pages, not for a script that roams the web.

`catpaw fetch` asks for no approval: you run it yourself, with the
actions you wrote on its command line.

## Be a good guest

CatPaw says what it is in its User-Agent (`CatPaw/<version>
(+https://catpaw.sh/bot)`); some sites refuse automated clients, and
CatPaw does not try to pass for something else or solve CAPTCHAs. Keep to
the site's terms and to a pace it can bear.
