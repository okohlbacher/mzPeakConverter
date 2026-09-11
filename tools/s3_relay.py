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

Above the 5 GB single-PUT ceiling, MANUAL only (box_convert.sh pulls a LOCAL target's archive by scp
instead; an s3:// target still stops at 5 GB -- see below):
    s3_relay.py presign-multipart <key> --parts N [--part-size B] [--expires S]
                                          # OPENS a multipart upload (server-side state!) and prints
                                          # {upload_id, part_size, urls:[...]} — one presigned PUT per
                                          # part. Every path out of it MUST end in complete- or
                                          # abort-multipart, or the parts sit in the bucket, billed.
    s3_relay.py complete-multipart <key> --upload-id ID --etags E1,E2,...   # -> assembled size
    s3_relay.py abort-multipart    <key> --upload-id ID                     # discard the parts
    s3_relay.py list-multipart     [prefix]                                 # -> open uploads (reap orphans)

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
    pmp = sub.add_parser("presign-multipart"); pmp.add_argument("key")
    pmp.add_argument("--parts", type=int, required=True)
    pmp.add_argument("--part-size", type=int, default=512 * 1024 * 1024)
    pmp.add_argument("--expires", type=int, default=21600)
    cmpm = sub.add_parser("complete-multipart"); cmpm.add_argument("key")
    cmpm.add_argument("--upload-id", required=True); cmpm.add_argument("--etags", required=True)
    amp = sub.add_parser("abort-multipart"); amp.add_argument("key")
    amp.add_argument("--upload-id", required=True)
    lmp = sub.add_parser("list-multipart"); lmp.add_argument("prefix", nargs="?", default="")
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
    elif a.cmd == "presign-multipart":
        # MANUAL ROUTE for an archive over the 5 GB single-PUT ceiling. The box has no credentials,
        # so each part needs its own presigned PUT. PXD077098's Waters TWIMS frame archive is
        # 9.04 GB and stopped the harness at `stage=too-big`; it was delivered by hand.
        #
        # This does NOT lift the harness's ceiling, and wiring it into box_convert.sh would not be
        # enough either: `copy` publishes the staging object with copy_object, which S3 caps at
        # 5 GB, and the deferred integrity gate compares the ETag against the body md5, which a
        # multipart object's ETag is not. So a hand-driven upload should address the DURABLE key
        # directly and be verified by size (see BACKLOG).
        if not 1 <= a.parts <= 10000:
            sys.exit("parts must be 1..10000 (S3 multipart limit)")
        if not 5 * 1024**2 <= a.part_size <= 5 * 1024**3:
            sys.exit("part-size must be 5 MiB..5 GiB (S3 multipart limits; the last part may be smaller)")
        r = s3.create_multipart_upload(Bucket=b, Key=a.key)
        urls = [s3.generate_presigned_url(
                    "upload_part",
                    Params={"Bucket": b, "Key": a.key, "UploadId": r["UploadId"], "PartNumber": i + 1},
                    ExpiresIn=a.expires)
                for i in range(a.parts)]
        print(json.dumps({"upload_id": r["UploadId"], "part_size": a.part_size, "urls": urls}))
    elif a.cmd == "complete-multipart":
        # ETags arrive in part order, comma separated, exactly as the uploader collected them.
        # REFUSE a blank field rather than skipping it: dropping one and numbering the rest 1..n
        # renumbers every following part, and S3 would happily assemble the shifted object.
        fields = [e.strip().strip('"') for e in a.etags.split(",")]
        for i, e in enumerate(fields):
            if not e:
                sys.exit(f"empty ETag for part {i + 1} of {len(fields)} — refusing to complete")
        parts = [{"ETag": e, "PartNumber": i + 1} for i, e in enumerate(fields)]
        s3.complete_multipart_upload(Bucket=b, Key=a.key, UploadId=a.upload_id,
                                     MultipartUpload={"Parts": parts})
        r = s3.head_object(Bucket=b, Key=a.key)
        # ETag of a multipart object is md5-of-part-md5s + "-N", NOT the body md5: check the size.
        print(json.dumps({"size": r["ContentLength"], "etag": r["ETag"].strip('"'), "parts": len(parts)}))
    elif a.cmd == "abort-multipart":
        s3.abort_multipart_upload(Bucket=b, Key=a.key, UploadId=a.upload_id)
    elif a.cmd == "list-multipart":
        # An interrupted presign-multipart leaves parts that are invisible to `ls` and still billed.
        pg = s3.get_paginator("list_multipart_uploads")
        for page in pg.paginate(Bucket=b, Prefix=a.prefix):
            for u in page.get("Uploads", []):
                print(f"{u['Initiated'].isoformat()}\t{u['UploadId']}\t{u['Key']}")
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
