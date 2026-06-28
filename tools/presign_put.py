#!/usr/bin/env python3
"""Mint a presigned S3 PUT URL so a remote runner can upload an output .mzpeak
straight into the bucket — no credentials on the runner.

Usage:
    presign_put.py <key> [--expires SECONDS] [--get] [--content-type CT]
    presign_put.py runs/run42/out.mzpeak --expires 86400

Prints the PUT URL (and, with --get, a GET URL for read-back). The runner uploads with:
    curl -fSL -X PUT --upload-file out.mzpeak "<PUT_URL>"
(add  -H "Content-Type: <CT>"  only if you minted the URL with --content-type)
"""
import argparse, sys
import boto3
from botocore.config import Config

ENDPOINT = "https://object.storage.eu01.onstackit.cloud"
REGION   = "EU-01"
BUCKET   = "v09"
PROFILE  = "stackit"

def client():
    return boto3.Session(profile_name=PROFILE).client(
        "s3", endpoint_url=ENDPOINT, region_name=REGION,
        config=Config(signature_version="s3v4", s3={"addressing_style": "path"}))

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("key", help="object key, e.g. runs/run42/out.mzpeak")
    ap.add_argument("--expires", type=int, default=3600, help="URL lifetime in seconds (default 3600)")
    ap.add_argument("--get", action="store_true", help="also print a presigned GET URL for read-back")
    ap.add_argument("--content-type", default=None,
                    help="pin Content-Type (runner must then send the matching header)")
    ap.add_argument("--bucket", default=BUCKET)
    a = ap.parse_args()
    s3 = client()
    params = {"Bucket": a.bucket, "Key": a.key}
    if a.content_type:
        params["ContentType"] = a.content_type
    put = s3.generate_presigned_url("put_object", Params=params, ExpiresIn=a.expires)
    print(put)
    if a.get:
        get = s3.generate_presigned_url(
            "get_object", Params={"Bucket": a.bucket, "Key": a.key}, ExpiresIn=a.expires)
        print(get, file=sys.stderr)  # GET on stderr so stdout stays the single PUT URL

if __name__ == "__main__":
    main()
