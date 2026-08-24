#!/usr/bin/env python3
"""Report whether a Shimadzu .lcd actually stores profile data, centroid data, or both.

A .lcd is an OLE2 compound file with a SYMMETRIC pair of raw-data streams:

    QTFL RawData/Profile Data    QTFL RawData/Centroid Data
    QTFL RawData/Profile Index   QTFL RawData/Centroid Index
    ...

Both are always PRESENT; the unused one is present at ZERO LENGTH. So the vendor API returning
an empty `ProfileList` is not evidence the reader is wrong -- read the stream sizes and settle it
without the vendor DLL, Windows, or a conversion.

    python3 tools/lcd_streams.py FILE.lcd [FILE.lcd ...]

Requires `olefile` (pip install olefile).
"""
import sys, os, olefile

def report(path):
    size = os.path.getsize(path)
    with olefile.OleFileIO(path) as ole:
        streams = {"/".join(s): ole.get_size(s) for s in ole.listdir(streams=True, storages=False)}
    print(f"{os.path.basename(path)}  ({size:,} B, {len(streams)} streams)")
    verdict = []
    for kind in ("Profile", "Centroid"):
        data = streams.get(f"QTFL RawData/{kind} Data")
        index = streams.get(f"QTFL RawData/{kind} Index")
        if data is None:
            print(f"  {kind:8} no such stream (not a QTOF .lcd?)")
            continue
        print(f"  {kind:8} Data {data:>15,} B   Index {index or 0:>10,} B"
              f"   {data / size * 100:5.1f}% of file")
        if data:
            verdict.append(kind.lower())
    print(f"  -> stores: {' + '.join(verdict) if verdict else 'NEITHER'}\n")
    return verdict

if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    for p in sys.argv[1:]:
        report(p)
