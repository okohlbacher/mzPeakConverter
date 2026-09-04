#!/usr/bin/env python3
"""S3 relay helpers for box-convert — presign a PUT (so the box can upload with no creds),
then GET / HEAD / DELETE from the host (which has the profile). StackIT S3-compatible by default;
override via env. No secrets in the file — auth comes from the named profile.

Subcommands:
    s3_relay.py presign-put <key> [--expires S] [--content-type CT]   # -> PUT url on stdout
    s3_relay.py get  <key> <dest>                                      # download object -> dest
    s3_relay.py head <key> [--etag]                                    # -> object size (bytes), or its ETag
    s3_relay.py presign-unit <key> [--expires S]                       # -> JSON {unit_key, primary,
                                                                       #    members:[{rel,url,size,mtime,etag}]}
                                                                       #    size/mtime/etag are the S3 OBJECT's
                                                                       #    (ContentLength/LastModified/ETag),
                                                                       #    not the host's local copy
    s3_relay.py delete <key>                                          # delete object (idempotent)
    s3_relay.py md5  <path>                                           # -> md5 of a local file (verify)

Env overrides: S3_BUCKET, S3_ENDPOINT, S3_REGION, AWS_PROFILE.
"""
import argparse, hashlib, json, os, re, sys

DEF_ENDPOINT = "https://object.storage.eu01.onstackit.cloud"
DEF_REGION = "EU-01"
DEF_BUCKET = "v09"
DEF_PROFILE = "stackit"


def client():
    import boto3
    from botocore.config import Config
    return boto3.Session(profile_name=os.environ.get("AWS_PROFILE", DEF_PROFILE)).client(
        "s3",
        endpoint_url=os.environ.get("S3_ENDPOINT", DEF_ENDPOINT),
        region_name=os.environ.get("S3_REGION", DEF_REGION),
        config=Config(signature_version="s3v4", s3={"addressing_style": "path"}),
    )


def bucket():
    return os.environ.get("S3_BUCKET", DEF_BUCKET)


def md5_of(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("presign-put")
    p.add_argument("key")
    p.add_argument("--expires", type=int, default=21600)  # 6 h: covers download+convert+upload
    p.add_argument("--content-type", default=None)
    pg = sub.add_parser("presign-get")
    pg.add_argument("key"); pg.add_argument("--expires", type=int, default=21600)
    pu = sub.add_parser("put"); pu.add_argument("key"); pu.add_argument("src")
    g = sub.add_parser("get"); g.add_argument("key"); g.add_argument("dest")
    h = sub.add_parser("head"); h.add_argument("key"); h.add_argument("--etag", action="store_true")
    pun = sub.add_parser("presign-unit"); pun.add_argument("key")
    pun.add_argument("--expires", type=int, default=21600)
    d = sub.add_parser("delete"); d.add_argument("key")
    m = sub.add_parser("md5"); m.add_argument("path")
    l = sub.add_parser("ls"); l.add_argument("prefix"); l.add_argument("--count", action="store_true")
    cp = sub.add_parser("copy"); cp.add_argument("src_key"); cp.add_argument("dst_key")
    a = ap.parse_args()

    if a.cmd == "md5":  # local-only, no network/boto3
        print(md5_of(a.path)); return

    s3 = client()
    b = bucket()
    if a.cmd == "presign-put":
        params = {"Bucket": b, "Key": a.key}
        if a.content_type:
            params["ContentType"] = a.content_type
        print(s3.generate_presigned_url("put_object", Params=params, ExpiresIn=a.expires))
    elif a.cmd == "presign-get":
        print(s3.generate_presigned_url("get_object", Params={"Bucket": b, "Key": a.key}, ExpiresIn=a.expires))
    elif a.cmd == "put":
        s3.upload_file(a.src, b, a.key)
    elif a.cmd == "get":
        s3.download_file(b, a.key, a.dest)
    elif a.cmd == "head":
        r = s3.head_object(Bucket=b, Key=a.key)
        # ETag of a SINGLE-part upload is the body's md5. The box PUTs via one presigned PUT (the
        # 5 GB ceiling enforces that), so this lets the host verify an upload without downloading it.
        print(r["ETag"].strip('"') if a.etag else r["ContentLength"])
    elif a.cmd == "copy":
        # Server-side: the bytes never touch this host. Used to publish a VERIFIED staging object to
        # its durable corpus key, so a corrupt or truncated upload can never appear at the real key.
        s3.copy_object(Bucket=b, Key=a.dst_key, CopySource={"Bucket": b, "Key": a.src_key})
        print(s3.head_object(Bucket=b, Key=a.dst_key)["ContentLength"])
    elif a.cmd == "presign-unit":
        # A vendor unit is rarely one object. It is either a PREFIX of many (.d, Waters .raw) or a
        # primary file plus SIDECARS that hold the actual payload (SCIEX .wiff + .wiff.scan + .wiff2,
        # imzML + .ibd -- VD_170826 is a 13 MB .wiff beside a 1.73 GB .scan). Emit rel->presigned-GET
        # for every member so the BOX reconstructs the unit and the bytes never cross the host.
        # rel is relative to the unit's PARENT, matching tar/_unit_members semantics, and `primary`
        # is what the converter must be handed. Exit 3 = not in the bucket.
        #
        # Each member also carries `size`, `mtime` (epoch seconds) and `etag`, and the unit carries a
        # stable `unit_key`. Those feed the box's PERSISTENT RAW CACHE (box_convert_remote.ps1): the
        # box keeps <cache>\<unit_key>\<rel> across runs and re-fetches any member whose cached
        # length, LastWriteTimeUtc or MD5 disagrees with what is declared here.
        # The identity is taken from the S3 OBJECT (ContentLength / LastModified / ETag), not from the
        # host's local copy: these are exactly the bytes the box will download, whereas a local
        # mtime moves on any rsync/checkout without the object changing at all.
        # `etag` is emitted ONLY for single-part uploads, where the ETag IS the body md5 -- the same
        # identity box_convert.sh's `head --etag` gate already trusts on the archive side. A multipart
        # ETag ("<hash>-<n>") is a hash of part hashes and would never match a file digest, so it is
        # dropped and that member falls back to the size+mtime check.
        #
        # POLICY: unit_key is minted ONLY for keys under the corpus bucket (unit_presign refuses
        # anything outside CORPUS_ROOT, and this command exits 3 for anything not in the bucket).
        # That is what keeps non-corpus material -- e.g. the Stephan Singer AGXT patient .d, which is
        # host-staged instead -- out of the box's persistent cache. Loosening that guard is a data
        # policy change, not just a plumbing one.
        BYPRODUCT = (".mzpeak", ".built", ".sig", ".partial", ".extracted", ".log", ".yaml", ".yml")

        def listp(prefix):
            objs, tok = [], None
            while True:
                kw = {"Bucket": b, "Prefix": prefix}
                if tok:
                    kw["ContinuationToken"] = tok
                r = s3.list_objects_v2(**kw)
                objs += r.get("Contents", [])
                if not r.get("IsTruncated"):
                    break
                tok = r["NextContinuationToken"]
            return [o for o in objs if not o["Key"].endswith("/")]

        def head_as_entry(k):  # same shape as a list_objects_v2 Contents item
            h = s3.head_object(Bucket=b, Key=k)
            return {"Key": k, "Size": h["ContentLength"], "LastModified": h["LastModified"],
                    "ETag": h.get("ETag", "")}

        def body_md5(o):  # "" unless the ETag is a real body md5 (single-part upload)
            e = (o.get("ETag") or "").strip('"')
            return "" if "-" in e else e

        key = a.key.rstrip("/")
        try:
            s3.head_object(Bucket=b, Key=key)
            exact = True
        except Exception:
            exact = False
        if exact:
            objs = listp(key)                        # key itself and any `key.<sidecar>`
            have = {o["Key"] for o in objs}
            stem = key.rsplit(".", 1)[0]
            for cand in (stem + ".wiff2", stem + ".ibd", stem + ".IBD"):
                if cand not in have:
                    try:
                        objs.append(head_as_entry(cand))
                    except Exception:
                        pass
        else:
            objs = listp(key + "/")
        arch = False
        if not objs:
            # Some corpus sources are stored ZIPPED beside their nominal key
            # (250501_ZMM_KMI_sFtsk_2.raw.zip). One object, and the box already sniffs .zip/.tgz,
            # so this is the cheapest source of all -- no host bytes, no multi-GET reconstruction.
            for ext in (".zip", ".tar", ".tgz", ".tar.gz"):
                try:
                    objs, arch = [head_as_entry(key + ext)], True
                    break
                except Exception:
                    pass
        objs = [o for o in objs if arch or not any(o["Key"].endswith(x) for x in BYPRODUCT)]
        if not objs:
            sys.exit(3)
        parent = key.rsplit("/", 1)[0] + "/" if "/" in key else ""
        # Stable per-unit cache identity: the corpus-relative key, never a presigned URL (those carry
        # a fresh signature every run). Readable prefix for debugging on the box + a hash so two units
        # with the same basename in different folders can never collide. Constrained to the character
        # class the box validates before it will touch the cache root.
        safe = re.sub(r"[^A-Za-z0-9._-]", "_", key[len(parent):])[:40] or "unit"
        unit_key = safe + "-" + hashlib.sha1(key.encode("utf-8")).hexdigest()[:12]
        print(json.dumps({
            "unit": key[len(parent):],
            "primary": key[len(parent):],
            "archive": arch,
            "unit_key": unit_key,
            "members": [{"rel": o["Key"][len(parent):],
                         "url": s3.generate_presigned_url(
                             "get_object", Params={"Bucket": b, "Key": o["Key"]}, ExpiresIn=a.expires),
                         "size": int(o["Size"]),
                         "mtime": int(o["LastModified"].timestamp()),
                         "etag": body_md5(o)} for o in objs]}))
    elif a.cmd == "ls":
        # Paginated: a vendor .d unit is hundreds of objects and list_objects_v2 caps at 1000.
        keys, tok = [], None
        while True:
            kw = {"Bucket": b, "Prefix": a.prefix}
            if tok:
                kw["ContinuationToken"] = tok
            r = s3.list_objects_v2(**kw)
            keys += [o["Key"] for o in r.get("Contents", [])]
            if not r.get("IsTruncated"):
                break
            tok = r["NextContinuationToken"]
        print(len(keys) if a.count else "\n".join(keys))
    elif a.cmd == "delete":
        s3.delete_object(Bucket=b, Key=a.key)  # idempotent: no error if absent


if __name__ == "__main__":
    main()
