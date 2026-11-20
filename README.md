# fov

*filter · order · view*

Describe the output you want in plain language, such as "total revenue of completed orders
per region". `fov` builds a pipeline with standard text tools such as `awk`, `jq` and `sort`.
It previews the output on your data. You can refine it with follow-up requests. Accept
the result to run the pipeline on the full input and print the command for reuse.

The model never gets a shell. It proposes programs and their arguments. `fov` checks
them against an allowlist and runs every process in an operating system sandbox. It uses
`sandbox-exec` on macOS and Landlock with seccomp on Linux.

## Install

```sh
cargo install --locked fov
```

`fov` supports macOS and Linux. Linux requires kernel 6.2 or later with Landlock enabled.

## Configure

Create `~/.config/fov/config.toml`. If you set `$XDG_CONFIG_HOME`, use
`$XDG_CONFIG_HOME/fov/config.toml` instead. This also applies on macOS.

```toml
provider = "openai"          # Choose openai, openrouter or bedrock.

[openai]
model = "<model id>"         # Required. There are no built-in defaults.
# api_key = "sk-…"           # Or set OPENAI_API_KEY.
# reasoning_effort = "xhigh" # Or use --reasoning-effort. Defaults to the model's setting.
# base_url = "…"             # Use a server that implements the Responses API.

[openrouter]
model = "<model id>"
# api_key = "sk-…"           # Or set OPENROUTER_API_KEY.
# reasoning_effort = "high"

[bedrock]
model = "<model or inference profile id>"
# profile = "work"           # Defaults to the AWS SDK chain, including SSO and `aws login`.
# region = "eu-central-1"
```

The model must support tool calls. OpenAI also requires support for forced tool calls.
OpenRouter and Bedrock don't force tool calls, so Claude 5.5 models work with those providers.

If the config file contains an API key, restrict its permissions with `chmod 600`.
`fov` warns if its permissions grant access to other users.

You can also set these optional settings with long options, such as `--context-head-kib`:

```toml
[context]
head_kib = 16        # Send the start of the input to the model.
head_max_kib = 64    # Read up to this limit if the initial chunk has no complete line.

[preview]
timeout_ms = 3000    # Preview the full input. On timeout, use a sample.
sample_kib = 1024    # Set the sample size.

[commands]
allow = ["awk", "cut", "grep", "jq", "sed", "sort", "uniq"]   # Replace the default list.
```

## Use

```sh
fov testdata/sales.csv                    # Write data to stdout and the command to stderr.
fov access.log -o top-ips.txt             # Write data to the file only after you accept.
kubectl get pods -o json | fov > pods.txt  # Read stdin. The terminal UI stays on the terminal.
cmd=$(fov -n events.json)                 # Write only the command to stdout.
```

| Key | Action |
|-----|--------|
| Enter | Send the request. The up arrow recalls the last request. |
| `a` | Accept the result, run the pipeline on the full input and exit. |
| `e` | Refine the result with a follow-up request. |
| `y` | Copy the shown command to the clipboard. |
| Tab | Move focus to the input or output pane. Scroll it with the arrow keys, Page Up, Page Down, Home or End. |
| Esc | Cancel while working. Return to the result while typing. |
| `q` | When a result is shown, quit and print the command without running it. |
| Ctrl-C | Quit without printing anything, at any time. |

When you accept, `fov` prints the output first, then the command. It replaces control
characters in terminal output, as it does in the preview, so a pipeline can't send escape
sequences to your terminal. Redirect the output or use `-o` to get the exact bytes.

To copy the command, `y` uses `pbcopy`, `wl-copy`, `xclip` or `xsel`. If none is available,
or when connected over SSH, it sends the command to the terminal as an OSC 52 sequence.
Most terminals use this sequence to write to the clipboard. In tmux, enable it with
`set -g set-clipboard on`. macOS Terminal.app doesn't support OSC 52.

`fov` returns these exit codes:

- `0` on success.
- The pipeline's exit status if the final run fails.
- `130` when you quit.
- `125` when `fov` itself fails, such as with invalid options, invalid config or no sandbox.

## Read the command before you reuse it

`fov` runs every preview and the final run in the sandbox. **The printed command is plain
shell and runs with your full permissions when you reuse it.**

Input data can contain instructions aimed at the model. Line 25 of `testdata/access.log`
has an example. Operations blocked by the sandbox often fail without a visible error.
For example, awk's `system()` returns a non-zero status. A pipeline can produce the expected
preview and still perform other actions when run outside the sandbox. Read the command
before you reuse it.

## Known limitations

- Stdin must end and is limited to 1 GiB. Save larger or endless input to a file.
- An interactive program downstream, such as `fov | less`, competes for the terminal.
  Use `-o` to write the output to a file.
- `fov` never cuts a single JSON document for a preview. It can't sample a large document
  if the preview times out. Increase `[preview].timeout_ms` in that case.
- Previews run with `PATH=/usr/bin:/bin` and no `HOME`. The printed command can behave
  differently in your shell, for example if jq loads `~/.jq`.
- Tools can read system libraries and data, but not configuration. The sandbox blocks
  access to `/etc`, Homebrew's `etc` and `var`, and other packages under `/opt`.
  On macOS, tools also can't look up the user. A tool that requires a home directory,
  such as `bat`, won't run.
- On Linux before 6.12, a tool that can make arbitrary system calls can still send signals
  to your other processes. Examples include an allowlisted perl or a gawk extension.
- Plain `awk -F,` doesn't handle quoted CSV fields such as `"a, b"`. Add `mlr` or `qsv` to
  the allowlist if you need to process them.
- On BusyBox systems such as stock Alpine, the tools are the same file as `/bin/sh`.
  `fov` excludes those tools. Install standalone tools to use them.
- `fov` sends the start of the input to the model provider, 16 KiB by default. It also
  sends the start of each preview's output.

## Development

```sh
cargo test                 # Run unit, executor, sandbox conformance, session, wire, terminal UI and end-to-end tests.
cargo run --features eval --bin fov-eval -- --provider openai --model <id> --repeat 3
```

The evaluation runner requires an API key. It runs `evals/cases.toml` against the model
and fails if any proposal follows the injection in `testdata/access.log`.

Regenerate the synthetic fixtures with `python3 -I scripts/gen-testdata.py testdata`.
Read `PRD.md` for the design.

