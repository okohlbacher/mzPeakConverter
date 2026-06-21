#!/usr/bin/env python3
"""Generate a CycloneDX 1.5 SBOM from `cargo metadata`.

Usage: cargo metadata --format-version 1 --all-features | python3 tools/gen_sbom.py > sbom.cdx.json

Captures every resolved dependency (all features) with name, version, purl, license, and source.
No third-party deps — pure stdlib so it runs anywhere cargo + python do.
"""
import json
import sys


def main() -> None:
    md = json.load(sys.stdin)
    root = md.get("resolve", {}).get("root")
    components = []
    for pkg in sorted(md["packages"], key=lambda p: (p["name"], p["version"])):
        if pkg["id"] == root:
            continue  # the application itself is the metadata.component, not a library
        comp = {
            "type": "library",
            "name": pkg["name"],
            "version": pkg["version"],
            "purl": f"pkg:cargo/{pkg['name']}@{pkg['version']}",
        }
        if pkg.get("description"):
            comp["description"] = pkg["description"]
        if pkg.get("license"):
            comp["licenses"] = [{"expression": pkg["license"]}]
        refs = []
        if pkg.get("repository"):
            refs.append({"type": "vcs", "url": pkg["repository"]})
        # source: registry => from crates.io; null => path/vendored/local
        src = pkg.get("source")
        if src is None:
            comp["properties"] = [{"name": "cdx:cargo:source", "value": "local/vendored (path)"}]
        if refs:
            comp["externalReferences"] = refs
        components.append(comp)

    root_pkg = next(p for p in md["packages"] if p["id"] == root)
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": root_pkg["name"],
                "version": root_pkg["version"],
                "purl": f"pkg:cargo/{root_pkg['name']}@{root_pkg['version']}",
            },
            "tools": [{"name": "gen_sbom.py", "vendor": "mzPeakConverter"}],
        },
        "components": components,
    }
    json.dump(bom, sys.stdout, indent=2, ensure_ascii=False)
    sys.stdout.write("\n")
    sys.stderr.write(f"SBOM: {len(components)} dependency components\n")


if __name__ == "__main__":
    main()
