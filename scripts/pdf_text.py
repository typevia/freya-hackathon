"""Extract bounded text from an input PDF; never execute input-folder code."""
import pathlib
import signal
import sys

if hasattr(signal, "alarm"):
    signal.alarm(20)
try:
    import resource
    resource.setrlimit(resource.RLIMIT_AS, (512 * 1024 * 1024, 512 * 1024 * 1024))
except ImportError:
    pass

root = pathlib.Path(__file__).resolve().parent.parent
wheels = list((root / "vendor").glob("pypdf-*.whl"))
if len(wheels) != 1:
    sys.exit("Exactly one pinned pypdf dependency is required")
sys.path.insert(0, str(wheels[0]))
from pypdf import PdfReader

source = pathlib.Path(sys.argv[1])
if source.stat().st_size > 16 * 1024 * 1024:
    sys.exit("PDF exceeds 16 MB input limit")
reader = PdfReader(str(source))
if reader.is_encrypted:
    sys.exit("Encrypted PDF requires an explicit text companion")
if len(reader.pages) > 500:
    sys.exit("PDF exceeds page limit")
parts = []
size = 0
for page in reader.pages:
    text = page.extract_text() or ""
    size += len(text.encode("utf-8")) + 2
    if size > 400_000:
        sys.exit("Extracted PDF text exceeds document context bound")
    parts.append(text)
text = "\n\n".join(parts)
if not text.strip():
    sys.exit("PDF has no extractable text; provide a text/OCR companion")
sys.stdout.write(text)
