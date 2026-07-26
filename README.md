# Nostra

A desktop chat client.

## Getting started

```sh
cargo run
```

Preferences are persisted to:

- macOS: `~/Library/Application Support/nostra/preferences.json`
- Linux: `$XDG_CONFIG_HOME/nostra/preferences.json`
  (falls back to `~/.config/nostra/preferences.json`)
- Windows: `%APPDATA%\nostra\preferences.json`

## Keyboard shortcuts

| Action           | macOS       | Others         |
| ---------------- | ----------- | -------------- |
| New chat         | ⌘N          | Ctrl+N         |
| Toggle sidebar   | ⌘B          | Ctrl+B         |
| Toggle theme     | ⌘⇧L         | Ctrl+Shift+L   |
| Quit             | ⌘Q          | Alt+F4         |

## Acknowledgements

Nostra has benefited from the architecture, compatibility work, and examples
in these open-source projects:

- [Zed](https://github.com/zed-industries/zed) and its GPUI framework
- [gpui-component](https://github.com/longbridge/gpui-component)
- [pi](https://github.com/earendil-works/pi)
- [Rig](https://github.com/0xPlaygrounds/rig)
- [one-api](https://github.com/songquanpeng/one-api)
- [Vercel AI SDK](https://github.com/vercel/ai)

## License

Licensed under the Apache License, Version 2.0 (the "License"); you may
not use this file except in compliance with the License.  See
[`LICENSE`](LICENSE) and [`NOTICE`](NOTICE) for details.

Copyright 2026 yuewei
