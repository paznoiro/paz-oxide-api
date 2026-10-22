#!/usr/bin/env python3
"""Verify a running packaged API against real Paddle and Indra models."""

import argparse
import json
import os
from pathlib import Path
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8080")
    parser.add_argument("--expect-math", choices=["true", "false"], default="true")
    args = parser.parse_args()
    key = os.environ["OXIDE_API_API_KEY"]
    fixtures = Path(__file__).resolve().parents[1] / "tests" / "fixtures"
    ready_deadline = time.monotonic() + 30
    while True:
        try:
            with urllib.request.urlopen(args.base_url.rstrip("/") + "/readyz", timeout=2) as response:
                assert json.load(response)["status"] == "ready"
            break
        except (urllib.error.URLError, TimeoutError):
            if time.monotonic() >= ready_deadline:
                raise
            time.sleep(0.1)

    def extract(endpoint, filename, query):
        request = urllib.request.Request(
            args.base_url.rstrip("/") + "/v1/extract/" + endpoint + "?" + query,
            data=(fixtures / filename).read_bytes(),
            headers={"Authorization": "Bearer " + key, "Accept": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=150) as response:
            return json.load(response)

    for engine in ["paddle", "indra"]:
        result = extract("document", "scan_en.pdf", f"ocr_engine={engine}&include_debug=true")
        assert result["ocr"]["engine"] == engine, result["ocr"]
        assert result["ocr"]["pages"] == [1], result["ocr"]
        assert "Invoice" in result["pages"][0]["text"], result["pages"][0]["text"]
        blocks = result["pages"][0]["blocks"]
        assert any(b["confidence"] is not None and b["bbox"] for b in blocks)
        steps = result["debug"]["steps"]
        assert steps[0]["step"] == "pdf_oxide_parse"
        assert steps[-1]["step"] == "assemble"
        if engine == "indra":
            cli = next(s for s in steps if s["step"] == "indra_cli")
            assert cli["input"]["math"] == (args.expect_math == "true")
            assert cli["output"]["pages"][0]["lines"]
            assert "file" not in cli["output"]["pages"][0]
        assert key not in json.dumps(result["debug"])
        print(f"{engine}: scanned PDF, positioned blocks, real confidence and flow trace passed")

    for endpoint, filename, selection in [
        ("text", "scan_en.png", ""),
        ("markdown", "scan_en.png", ""),
        ("text", "scan_pages.tif", "&pages=2"),
    ]:
        result = extract(endpoint, filename, "ocr_engine=indra&include_debug=true" + selection)
        assert result["text"].strip(), result
        assert result["ocr"]["engine"] == "indra"
        if selection:
            assert result["pages_extracted"] == [2], result["pages_extracted"]
        print(f"indra: {endpoint} {filename} passed")

    native = extract("document", "native.pdf", "ocr_engine=indra&include_debug=true")
    assert not native["ocr"]["pages"], native["ocr"]
    assert not any(s["step"] == "indra_cli" for s in native["debug"]["steps"])
    print("native PDF: OCR skipped passed")


if __name__ == "__main__":
    main()
