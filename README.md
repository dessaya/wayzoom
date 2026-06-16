# wayzoom

A small **view-only** screen magnifier for Wayland.

Requires a compositor that supports `wlr-layer-shell`, `wlr-screencopy`, and
`wp_viewporter` (most wlroots-based compositors and similar).

It maps a fullscreen overlay with a colored border, grabs a single frozen frame of
the current output via `wlr-screencopy`, and lets you pan/zoom it with the mouse.
While the overlay is up it grabs input — you can't interact with apps underneath
(that's what the border reminds you of). Scaling is done by the compositor via
`wp_viewporter`, so panning stays smooth.

> View-only by design: live same-output magnification is impossible with
> `wlr-screencopy` (the overlay would capture itself), so the frame is frozen.

## Controls

| Input            | Action      |
| ---------------- | ----------- |
| Mouse wheel up   | Zoom in     |
| Mouse wheel down | Zoom out    |
| Move mouse       | Pan         |
| `Esc`            | Quit        |

## Build & run

```sh
cargo run --release            # or: cargo run --release -- --no-border
```

Options:

- `--no-border` — don't draw the reminder border around the overlay.

## Toggling from a shortcut

There is one instance per output. Launching wayzoom again for an output that
already has one signals the running instance to exit (the same graceful zoom-out as
`Esc`), so you can bind a single global shortcut to both open and close it.
