"""Run the upstream patcher only against the assistant's disposable app copy."""
import importlib.util
from pathlib import Path, PurePosixPath
import stat
import sys
import tempfile
import zipfile


def extract_archive(archive, destination):
    destination = Path(destination)
    if destination.exists() or destination.is_symlink():
        raise RuntimeError("Engine extraction destination already exists")
    with zipfile.ZipFile(archive) as source:
        members = source.infolist()
        if not members:
            raise RuntimeError("Empty engine archive")
        for member in members:
            name = member.orig_filename
            path = PurePosixPath(name)
            if (not name or path.is_absolute() or ".." in path.parts
                    or "\\" in name
                    or stat.S_ISLNK(member.external_attr >> 16)):
                raise RuntimeError("Unsafe engine archive entry")
        source.extractall(destination)


def main():
    script, app, work, mode = sys.argv[1:]
    work = Path(work).resolve(strict=True)
    app = Path(app).resolve(strict=True)
    if app.parent != work or app.name != "Claude.app" or mode != "safe":
        raise RuntimeError("Invalid isolated patch target")
    temp = work / "temporary"
    temp.mkdir()
    tempfile.tempdir = str(temp)
    spec = importlib.util.spec_from_file_location("claude_upstream", script)
    upstream = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(upstream)
    for name in ("main", "quit_claude", "set_user_locale", "verify", "backup_and_replace"):
        if not callable(getattr(upstream, name, None)):
            raise RuntimeError(f"Upstream interface changed: {name}")
    # The Rust caller owns process shutdown and changes the real locale only
    # after the patched bundle passed verification and was installed.
    upstream.quit_claude = lambda: None
    upstream.set_user_locale = lambda *_args, **_kwargs: None
    sys.argv = [script, "--app", str(app), "--user-home", str(work), "--lang", "zh-CN", "--skip-asar-patch"]
    return upstream.main()


if __name__ == "__main__":
    if sys.argv[1:2] == ["--extract"]:
        extract_archive(*sys.argv[2:])
    else:
        raise SystemExit(main())
