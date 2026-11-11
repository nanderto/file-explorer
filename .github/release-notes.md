### Install

1. Download the `.dmg`, open it, and drag **File Explorer** to Applications.
2. Open it once. macOS will refuse, because this build is **not notarised**.
3. Go to **System Settings → Privacy & Security**, scroll to **Security**, and
   click **Open Anyway** next to the message about File Explorer. Confirm.

You only need to do that once.

> Right-click → Open does **not** work for this. macOS 15 removed that bypass
> for software that is not notarised, so System Settings is the only route.

If you would rather use the Terminal, this does the same thing in one step:

```sh
xattr -dr com.apple.quarantine "/Applications/File Explorer.app"
```

**Why the warning?** Notarising requires a paid Apple Developer Program
membership. The app *is* ad-hoc signed, so macOS can verify it has not been
tampered with since it was built — Apple simply has not vouched for it, and
macOS says so.

---

See the [README](https://github.com/nanderto/file-explorer#readme) for what it
does and the keyboard shortcuts.
