"""Extract the MinGW-w64 archive used for the GNU toolchain's `dlltool`/`windres`.

Usage: python extract_toolchain.py <archive.7z> <destination-dir>
"""
import sys
import pathlib

import py7zr


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    source = pathlib.Path(sys.argv[1])
    dest = pathlib.Path(sys.argv[2])
    if not source.exists():
        print(f"archive not found: {source}")
        return 1
    dest.mkdir(parents=True, exist_ok=True)
    with py7zr.SevenZipFile(source, "r") as archive:
        names = archive.getnames()
        archive.extractall(path=str(dest))
    print(f"extracted {len(names)} entries into {dest}")
    for wanted in ("dlltool.exe", "windres.exe", "ld.exe", "gcc.exe"):
        hits = list(dest.rglob(wanted))
        print(f"  {wanted}: {'FOUND ' + str(hits[0]) if hits else 'MISSING'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
