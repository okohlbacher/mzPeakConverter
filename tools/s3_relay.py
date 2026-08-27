#!/usr/bin/env python3
"""S3 relay helpers for box-convert — presign a PUT (so the box can upload with no creds),
then GET / HEAD / DELETE from the host (which has the profile). StackIT S3-compatible by default;
override via env. No secrets in the file — auth comes from the named profile.

Subcommands:
    s3_relay.py presign-put <key> [--expires S] [--content-type CT]   # -> PUT url on stdout
    s3_relay.py get  <key> <dest>                                      # download object -> dest
    s3_relay.py head <key>                                            # -> object size (bytes) on stdout
    s3_relay.py delete <key>                                          # delete object (idempotent)
    s3_relay.py md5  <path>                                           # -> md5 of a local file (verify)

Env overrides: S3_BUCKET, S3_ENDPOINT, S3_REGION, AWS_PROFILE.
"""
import argparse, hashlib, os, sys

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
    h = sub.add_parser("head"); h.add_argument("key")
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
        print(s3.head_object(Bucket=b, Key=a.key)["ContentLength"])
    elif a.cmd == "copy":
        # Server-side: the bytes never touch this host. Used to publish a VERIFIED staging object to
        # its durable corpus key, so a corrupt or truncated upload can never appear at the real key.
        s3.copy_object(Bucket=b, Key=a.dst_key, CopySource={"Bucket": b, "Key": a.src_key})
        print(s3.head_object(Bucket=b, Key=a.dst_key)["ContentLength"])
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
