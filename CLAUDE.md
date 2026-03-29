# Scroll

Rust feature clone of [brrowser](https://github.com/isene/brrowser), a terminal web browser.

Terminal web browser with vim-style keys, inline images, tabs, forms, bookmarks, and AI summaries. Built on Crust.

## Build

```bash
PATH="/usr/bin:$PATH" cargo build --release
```

Note: `PATH` prefix needed to avoid `~/bin/cc` (Claude Code sessions) shadowing the C compiler.
