# Coming from Ranger?

Favnyr was previously named Ranger, renamed to avoid a name clash with an unrelated, [well-established terminal file manager](https://github.com/ranger/ranger).

Settings, favourites, workspaces and custom commands are plain files in a per-user folder named after the app, so the new version starts with none of them.

Before launching Favnyr for the first time, you may want to transfer your settings and workspaces as shown below (you can also overwrite them later) :

**Windows** — config and workspace data share one folder. Press <kbd>Win</kbd>+<kbd>R</kbd>, type `%APPDATA%`, press Enter, then rename the `ranger` folder you find there to `favnyr`.

**Linux** — two folders to rename :

```sh
mv ~/.config/ranger ~/.config/favnyr
mv ~/.local/share/ranger ~/.local/share/favnyr
```

Skipping this is entirely safe : Favnyr just starts fresh with default settings. **Settings → Storage locations** shows the exact new paths once it is running, if you want to double-check afterwards.

I didn't want to automate this migration because there might always be specific cases (another folder named "ranger" coming from a different project...).

# Where is Favnyr ?
Here => https://github.com/GlitchAwakened/Favnyr
