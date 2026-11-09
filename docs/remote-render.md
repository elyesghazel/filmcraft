# Render on another machine

Export a sequence on another computer you can reach over SSH: a desktop with a bigger GPU, a
server, any machine on the same Tailscale network. The file comes back to the path you chose, as
if it had been exported here.

## Use it

- **App:** Export mode ▸ **Render on**: pick a host from your `~/.ssh/config` or type one
  (`pc`, `user@server`, `100.64.0.2`), then **Export**. The job shows in the queue like any
  export, with progress, and **Cancel** stops the render on the other machine too.
- **CLI:** `filmcraft-cli --project cut.fcproj export cut.mp4 --preset "…" --on pc`
  (`--remote-cli /path/to/filmcraft-cli` if it is not on the host's `PATH`).
- **Agents (MCP, control channel):** `file.exportMediaRemote` with the same parameters as
  `file.exportMedia` plus `host`. `dryRun: true` reports how many files and bytes would be sent.

## What the host needs

- SSH that logs in without a prompt (`ssh -o BatchMode=yes <host> true` must work): a key in
  its `authorized_keys`, and its host key already in your `known_hosts`.
- `rsync` on both machines.
- `filmcraft-cli` on the host's `PATH` or in `~/.local/bin`, ideally the same version as here
  (an older one renders without a progress bar). Its login shell may be bash, zsh or fish.

## How it works

1. The export settings and range are resolved here, exactly as a local export resolves them, so
   user presets work even though the host doesn't have them.
2. The media the sequence uses (following nested sequences and subclips) is copied to
   `~/.cache/filmcraft-remote/media/` on the host. Each file is named by a fingerprint of its
   path, size and modification time, so it crosses the network once: rendering the same project
   again only sends what changed. Copies arrive as `.part` files and are renamed when complete.
3. A copy of the project is written with its used media pointing at those files. Unused media
   and proxies are left out; the render always reads the originals.
4. The host runs `filmcraft-cli --progress --project … exec file.exportMedia …`. Its
   `progress <0..1>` lines drive the progress bar here.
5. The result is copied back next to a `.part` name and renamed, and the job's folder on the
   host is deleted (the media cache stays for the next render).

Generated media (bars, mattes, demo footage) needs no upload. Image-sequence media and
image-sequence exports are not supported yet.

## `--progress`

`filmcraft-cli --progress` is useful on its own: any long command (`export`, `exec
file.exportMedia`, `run` scripts) prints `progress <fraction>` lines on stderr while it runs.
