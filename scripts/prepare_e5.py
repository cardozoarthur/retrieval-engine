"""Explicit, pinned CPU asset setup. No credentials or model download at inference."""
import hashlib
import io
import json
import platform
import shutil
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEST = ROOT / 'assets' / 'e5-small-int8'
REV = '761b726dd34fb83930e26aab4e9ac3899aa1fa78'
BASE = f'https://huggingface.co/Xenova/multilingual-e5-small/resolve/{REV}/'
FILES = {
    'model.onnx': ('onnx/model_int8.onnx', '4d24e2bc01a447951524466ef533e52944bf48509e6552810bcee1a2711cb02c', 118054593),
    'tokenizer.json': ('tokenizer.json', '0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39', 17082730),
}
RUNTIME = 'https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-win-x64-1.22.0.zip'
RUNTIME_HASHES = {
    'onnxruntime.dll': '579b636403983254346a5c1d80bd28f1519cd1e284cd204f8d4ff41f8d711559',
    'onnxruntime_providers_shared.dll': 'ba00ea1ef846c9b909c7854bc56c51051a20f9773b3e1153dda118d4b85d0b93',
}


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def get(url):
    return urllib.request.urlopen(urllib.request.Request(url, headers={'User-Agent': 'retrieval-engine-setup/1'}), timeout=90)


def main():
    if platform.system() != 'Windows' or platform.machine().lower() not in ['amd64', 'x86_64']:
        raise RuntimeError('Current pinned native bundle targets Windows x64')
    needs_download = not (DEST.joinpath('manifest.json').exists() and all(DEST.joinpath(name).exists() for name in FILES))
    if needs_download and shutil.disk_usage(ROOT).free < 2 * 1024**3:
        raise RuntimeError('Need at least 2 GiB free before setup; shared build volume')
    DEST.mkdir(parents=True, exist_ok=True)
    recorded = {}
    for name, (remote, expected, size) in FILES.items():
        path = DEST / name
        if path.exists():
            if path.stat().st_size != size or digest(path) != expected:
                raise RuntimeError(f'Existing {name} failed verification; refusing overwrite')
        else:
            partial = path.with_suffix(path.suffix + '.part')
            with get(BASE + remote) as response, partial.open('wb') as out:
                count = 0
                while block := response.read(1024 * 1024):
                    count += len(block)
                    if count > size:
                        raise RuntimeError(f'Unexpected size: {name}')
                    out.write(block)
            if count != size or digest(partial) != expected:
                raise RuntimeError(f'Download hash mismatch: {name}')
            partial.replace(path)
        recorded[name] = {'sha256': expected, 'bytes': size, 'source': BASE + remote}
        print(f'Verified {name}: {size} bytes', flush=True)

    manifest_path = DEST / 'manifest.json'
    prior = json.loads(manifest_path.read_text('utf-8')) if manifest_path.exists() else None
    if prior:
        if not set(RUNTIME_HASHES).issubset(prior['files']):
            raise RuntimeError('Installed runtime manifest is incomplete')
        for name, value in prior['files'].items():
            if name.endswith('.dll'):
                if name not in RUNTIME_HASHES or value['sha256'] != RUNTIME_HASHES[name] or digest(DEST / name) != RUNTIME_HASHES[name]:
                    raise RuntimeError('Installed runtime hash mismatch')
                recorded[name] = value
    else:
        # Keep the archive in bounded memory; install only the necessary DLLs and
        # licensing files, not a second full native SDK or a retained installer.
        with get(RUNTIME) as response:
            archive = response.read(80 * 1024 * 1024 + 1)
        if len(archive) > 80 * 1024 * 1024:
            raise RuntimeError('Runtime archive exceeds budget')
        with zipfile.ZipFile(io.BytesIO(archive)) as zipped:
            for member in zipped.infolist():
                name = Path(member.filename).name
                if name not in ['onnxruntime.dll', 'onnxruntime_providers_shared.dll', 'LICENSE', 'ThirdPartyNotices.txt']:
                    continue
                if member.file_size > 64 * 1024 * 1024:
                    raise RuntimeError('Runtime member exceeds budget')
                path = DEST / name
                data = zipped.read(member)
                if name.endswith('.dll') and hashlib.sha256(data).hexdigest() != RUNTIME_HASHES[name]:
                    raise RuntimeError('Official runtime member hash mismatch')
                path.write_bytes(data)
                if name.endswith('.dll'):
                    recorded[name] = {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data), 'source': RUNTIME}
        if 'onnxruntime.dll' not in recorded:
            raise RuntimeError('Official archive omitted runtime DLL')
    manifest = {'schema_version': 1, 'model': 'intfloat/multilingual-e5-small',
        'conversion_repository': 'Xenova/multilingual-e5-small', 'revision': REV,
        'license': 'MIT', 'runtime_version': '1.22.0', 'weight_format': 'int8',
        'dimensions': 384, 'max_tokens': 512, 'files': recorded}
    manifest_path.write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    print('Installed one CPU asset set; no archive retained', flush=True)


if __name__ == '__main__':
    main()
