Probe the host for bind-mount candidates (gh CLI token, gitconfig, ssh-agent socket, work-private bare-repo paths, etc.) and write / update the operator's `examples/compose/docker-compose.override.yml` with the detected mounts. Re-runnable: parses the existing override (if any), proposes adds / removes / changes, confirms with the operator, then writes — never blows the file away.

## When to invoke

- **First-time setup**: a fresh cw stack has no override file. The operator wants `gh` and `git` to work inside the container, plus access to any work-private repos that aren't under `~/repos/`. Run this skill to probe for the standard candidates and write an initial override.
- **New host path discovered**: the in-container claude realizes it needs a host path that isn't currently mounted (e.g. operator says "I keep my work scripts in `/Users/x/work/scripts`, mount that"). Run this skill with the new path; it will update the existing override and remind the operator to recreate.
- **Cleanup after a host reorg**: the operator moved their gh token dir, deleted a stale Google Drive bare-repo path, etc. Run this skill to re-probe + remove obsolete mounts from the override.

## Steps

1. **Probe `host-bash` availability**: `claude mcp list` should show `host-bash` as `Connected`. If it's not connected, **stop and tell the operator** — this skill needs `host-bash` to probe the host filesystem; without it, you'd be guessing host paths blindly. The operator either needs to wire up host-bash (`mcp-host-bash` launcher) or hand-edit the override file from the template at `examples/compose/docker-compose.override.yml.example`.

2. **Read the existing override** (if any). The override lives at `~/repos/claude-watch/examples/compose/docker-compose.override.yml` (host-side path; the file isn't bind-mounted into the container by default — read it via `host-bash cat`). Parse out the currently-mounted host paths so step 4 can diff against them.

3. **Probe the host for standard candidates**. Use `host-bash` for each of the following; record which exist on this host. The probes are cheap (each is a single `ls` / `test`), so probe all of them even if some are obviously not relevant.

   | Candidate | Host probe | If present, mount as |
   | --- | --- | --- |
   | gh CLI token | `test -f ${HOME}/.config/gh/hosts.yml` | `${HOME}/.config/gh:/home/hndrewaall/.config/gh:rw` |
   | gitconfig | `test -e ${HOME}/.gitconfig` (file or symlink) | `${HOME}/.gitconfig:/home/hndrewaall/.gitconfig:ro` |
   | ssh-agent socket (Docker Desktop magic) | `test -S /run/host-services/ssh-auth.sock` | `/run/host-services/ssh-auth.sock:/run/host-services/ssh-auth.sock` + env `SSH_AUTH_SOCK=/run/host-services/ssh-auth.sock` |
   | ssh-agent socket (Linux) | `echo $SSH_AUTH_SOCK; test -S "$SSH_AUTH_SOCK"` | `<host-socket>:/run/host-services/ssh-auth.sock` + env `SSH_AUTH_SOCK=/run/host-services/ssh-auth.sock` |
   | ssh config + known_hosts | `test -f ${HOME}/.ssh/config && test -f ${HOME}/.ssh/known_hosts` | `${HOME}/.ssh/config:/home/hndrewaall/.ssh/config:ro` + `${HOME}/.ssh/known_hosts:/home/hndrewaall/.ssh/known_hosts:ro` |
   | macOS Google Drive work-repos | `ls "${HOME}/Library/CloudStorage/" 2>/dev/null \| grep -i googledrive` then probe each for a `work-repos` / `bare-repos` subdir | `${HOME}/Library/CloudStorage/<drive>/My Drive/work-repos:/home/hndrewaall/work-repos:rw` |
   | External SSD project paths | (operator-supplied; ask before probing) | `<host-path>:/home/hndrewaall/<dest>:rw` |

   For Linux ssh-agent, the magic Docker Desktop path doesn't exist. Use the host's actual `$SSH_AUTH_SOCK` as the LEFT side of the bind-mount, but keep the in-container destination at `/run/host-services/ssh-auth.sock` for cross-host consistency (matches the env var default). If the host doesn't have an agent running at all, skip this candidate and tell the operator they need to start one before SSH-based git pushes will work.

4. **Diff against the current override**. Three categories:
   - **Add**: candidate is present on the host, NOT in the override.
   - **Remove**: in the override, candidate path NO LONGER exists on the host (operator deleted / moved it).
   - **Keep**: in the override, still present on the host. No-op.

   Skip the "remove" category for the ssh-agent socket if the host is Linux and `$SSH_AUTH_SOCK` is unset for this `host-bash` invocation — the operator may have an agent running interactively but not in cron / non-login shells. Ask before removing.

5. **Confirm with the operator**. Print a short proposal like:

   ```
   Proposed override updates:
     ADD    ~/.config/gh        (gh CLI token; enables `gh auth status`)
     ADD    ~/.gitconfig         (git identity)
     ADD    ssh-agent socket    (enables `ssh git@github.com`)
     KEEP   ~/work-repos        (already mounted)
     REMOVE ~/old-projects      (no longer present on host)

   Apply? [y/N]
   ```

   Wait for the operator's confirmation before writing. If the operator wants to add a path that wasn't auto-detected (e.g. `/Users/x/work/scripts:/home/hndrewaall/work-scripts:rw`), accept the explicit instruction and merge it into the proposal.

6. **Write the override**. Build the final mount list (existing kept + adds, minus removes), then write the new override file via `host-bash`. Preserve the existing comments in the file where possible — when in doubt, regenerate from the canonical template at `examples/compose/docker-compose.override.yml.example` and inject the resolved mount lines. The override goes at the host path `~/repos/claude-watch/examples/compose/docker-compose.override.yml` (the same file `docker compose` auto-merges).

7. **Tell the operator to recreate**. Bind-mount changes don't apply on a plain `up -d`; the container has to be recreated. Print:

   ```
   Override updated. Apply with:
     cd ~/repos/claude-watch/examples/compose
     docker compose up -d --force-recreate claude-container
   ```

   You can offer to run that recreate via `host-bash` if the operator wants — but ASK first; recreating the container terminates the operator's current in-container session.

## Important

- **The override file is gitignored.** It's in `.gitignore` (`examples/compose/docker-compose.override.yml`) precisely because it leaks personal paths. Don't suggest committing it; don't `git add` it.
- **The canonical template is `docker-compose.override.yml.example`** (committed). When generating a fresh override from scratch (no existing file), start from that template's structure so the comments + uncomment-pattern stay consistent across operators.
- **Keep the in-container destination paths stable**. Even when the host source path changes (Linux vs macOS, `${HOME}` vs `/Users/x`), the in-container destination should stay the same so the rest of the container's tooling doesn't have to switch on host OS. Standard destinations: `/home/hndrewaall/.config/gh`, `/home/hndrewaall/.gitconfig`, `/run/host-services/ssh-auth.sock`, `/home/hndrewaall/work-repos`, etc.
- **No private keys.** The ssh-agent socket forwards key signing to the host agent — never bind-mount `~/.ssh/id_*` private keys themselves. If the operator asks you to mount a private key directly, push back and explain the agent-forwarding alternative.
- **`host-bash` is a window to the host, not the host.** When reporting what you did, frame it as "I wrote the override on the host via host-bash" — the orchestration runs in the container, the file write executes on the host.
