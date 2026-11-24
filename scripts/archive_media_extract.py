"""Private bounded worker for archive_media.py; never prints document diagnostics.

Local readers: pypdf (PDF), xlrd (BIFF .xls), olefile (legacy PowerPoint),
legacy-doc (Word 97–2003). PyMuPDF, pdftotext and system Word readers remain optional.
Images use Windows' installed OCR languages, macOS Vision, or Pillow + Tesseract on Linux.
No converters that execute macros or fetch external links are used.
"""

import io
import os
from pathlib import Path
import posixpath
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
import zipfile


_MAX_OUTPUT = 256 << 10
_MAX_XML = 8 << 20
_MAX_TOTAL_XML = 64 << 20
_MAX_ENTRIES = 4096
_MAX_PAGES = 256
_MAX_PIXELS = 40_000_000
_OLE = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"
_SWIFT = Path(__file__).with_suffix(".swift").resolve()


class Rejected(Exception):
    pass


class _Full(Exception):
    pass


class _Text:
    def __init__(self):
        self.data = io.BytesIO()

    def add(self, text, sep="\n"):
        if not text:
            return
        left = _MAX_OUTPUT - self.data.tell()
        # Slice before encoding to avoid allocating output for huge XML cells.
        chunk = (text[:left] + sep).replace("\x00", "").encode("utf-8", "replace")
        if len(chunk) >= left:
            self.data.write(chunk[:left].decode("utf-8", "ignore").encode("utf-8"))
            raise _Full
        self.data.write(chunk)


class _Tree(ET.TreeBuilder):
    depth = 0

    def start(self, tag, attrs):
        self.depth += 1
        if self.depth > 128:
            raise Rejected("xml_depth_limit")
        return super().start(tag, attrs)

    def end(self, tag):
        self.depth -= 1
        return super().end(tag)

    def doctype(self, *_args):
        raise Rejected("xml_doctype_forbidden")


def _local(tag):
    return tag.rsplit("}", 1)[-1]


def _elements(part, tag):
    return (node for node in part.iter() if _local(node.tag) == tag)


def _attr(node, tag):
    return next((value for key, value in node.attrib.items() if _local(key) == tag), None)


class _Office:
    def __init__(self, path):
        # Reject huge/ZIP64 central directories before ZipFile builds its in-memory index.
        with open(path, "rb") as stream:
            size = stream.seek(0, os.SEEK_END)
            start = max(0, size - 65557)
            stream.seek(start)
            tail = stream.read()
        offset = tail.rfind(b"PK\x05\x06")
        if offset < 0 or len(tail) - offset < 22:
            raise Rejected("invalid_zip_directory")
        disk, cd_disk, on_disk, entries, directory_size, directory_offset, comment = struct.unpack_from("<HHHHIIH", tail, offset + 4)
        if (disk or cd_disk or on_disk != entries or entries > _MAX_ENTRIES
                or directory_size > (4 << 20) or directory_offset + directory_size != start + offset
                or offset + 22 + comment != len(tail)):
            raise Rejected("zip_directory_limit")
        self.zip = zipfile.ZipFile(path)
        self.remaining = _MAX_TOTAL_XML

    def xml(self, name):
        member = self.zip.getinfo(name)
        if member.file_size > min(_MAX_XML, self.remaining):
            raise Rejected("xml_size_limit")
        self.remaining -= member.file_size
        with self.zip.open(member) as handle:
            payload = handle.read(member.file_size + 1)
        if len(payload) != member.file_size:
            raise Rejected("xml_size_mismatch")
        return ET.fromstring(payload, parser=ET.XMLParser(target=_Tree()))

    def ordered_parts(self, document, relationship_part, tag, prefix):
        available = set(self.zip.namelist())
        targets = {}
        for node in _elements(self.xml(relationship_part), "Relationship"):
            target = node.get("Target", "")
            if node.get("TargetMode", "").lower() == "external" or not target:
                continue
            name = posixpath.normpath(target[1:] if target.startswith("/")
                                        else posixpath.join(posixpath.dirname(document), target))
            if name.startswith(prefix) and name in available and name.endswith(".xml"):
                targets[node.get("Id")] = name
        for node in _elements(self.xml(document), tag):
            # Slide IDs also have an unqualified numeric id; r:id is the relationship.
            rel = next((v for k, v in node.attrib.items() if k.startswith("{") and _local(k) == "id"), None)
            if rel in targets:
                yield node.get("name"), targets[rel]


def _paragraphs(doc, text):
    for paragraph in _elements(doc, "p"):
        text.add("".join((node.text or "") if _local(node.tag) == "t" else "\t" if _local(node.tag) == "tab" else "\n"
                         for node in paragraph.iter() if _local(node.tag) in ("t", "tab", "br")))


def _excel(office, text):
    shared = []
    if "xl/sharedStrings.xml" in office.zip.NameToInfo:
        shared = ["".join(node.text or "" for node in _elements(item, "t"))
                  for item in _elements(office.xml("xl/sharedStrings.xml"), "si")]
    for name, part in office.ordered_parts("xl/workbook.xml", "xl/_rels/workbook.xml.rels", "sheet", "xl/worksheets/"):
        text.add(name)
        for row in _elements(office.xml(part), "row"):
            for cell in row:
                if _local(cell.tag) != "c":
                    continue
                kind = cell.get("t")
                value = next(_elements(cell, "v"), None)
                data = value.text if value is not None else None
                if kind == "s" and data is not None:
                    index = int(data)
                    if not 0 <= index < len(shared):
                        raise Rejected("invalid_shared_string")
                    data = shared[index]
                elif kind == "inlineStr":
                    data = "".join(node.text or "" for node in _elements(cell, "t"))
                elif kind == "b" and data in ("0", "1"):
                    data = "TRUE" if data == "1" else "FALSE"
                elif data is None:
                    formula = next(_elements(cell, "f"), None)
                    if formula is not None and formula.text:
                        data = "=" + formula.text
                text.add(data, "\t")
            text.add("\n", "")


def _office(path, text):
    office = _Office(path)
    with office.zip:
        names = office.zip.namelist()
        if "word/document.xml" in names:
            _paragraphs(office.xml("word/document.xml"), text)
            for name in sorted(names):
                if re.fullmatch(r"word/(?:header\d+|footer\d+|footnotes|endnotes)\.xml", name):
                    _paragraphs(office.xml(name), text)
        elif "xl/workbook.xml" in names:
            _excel(office, text)
        elif "ppt/presentation.xml" in names:
            for _, part in office.ordered_parts("ppt/presentation.xml", "ppt/_rels/presentation.xml.rels", "sldId", "ppt/slides/"):
                _paragraphs(office.xml(part), text)
        else:
            return False
        return True


def _program(name):
    executable = shutil.which(name)
    if executable is None:
        raise Rejected("missing:" + name)
    return executable


def _native(text, *argv):
    with tempfile.TemporaryFile(dir=".") as output:
        result = subprocess.run([_program(argv[0]), *argv[1:]], stdin=subprocess.DEVNULL,
                                stdout=output, stderr=subprocess.DEVNULL)
        if result.returncode:
            if result.returncode == 4 and argv[0] == "swift":
                raise Rejected("image_or_page_limit")
            raise Rejected("reader_failed:" + argv[0])
        output.seek(0)
        text.add(output.read(_MAX_OUTPUT + 1).decode("utf-8", "replace"), "")


def _pdf(path, text):
    try:
        from pypdf import PdfReader
    except ImportError:
        pass
    else:
        with open(path, "rb") as stream:
            doc = PdfReader(stream)
            if doc.is_encrypted:
                raise Rejected("encrypted_pdf")
            for index in range(min(len(doc.pages), _MAX_PAGES)):
                text.add(doc.pages[index].extract_text())
        return
    try:
        import fitz
    except ImportError:
        if shutil.which("pdftotext"):
            _native(text, "pdftotext", "-l", str(_MAX_PAGES), path, "-")
        elif sys.platform == "darwin":
            _native(text, "swift", str(_SWIFT), "pdf", path)
        else:
            raise Rejected("missing:pdf_reader")
        return
    with fitz.open(path, filetype="pdf") as doc:
        if doc.needs_pass:
            raise Rejected("encrypted_pdf")
        for page in range(min(len(doc), _MAX_PAGES)):
            text.add(doc[page].get_text())


# Windows PowerShell 5 projects the inbox WinRT OCR API. Pass the file path
# through the environment, never interpolate attachment metadata into a command.
_WIN_OCR = r"""
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Storage.StorageFile,Windows.Storage,ContentType=WindowsRuntime]
$null = [Windows.Storage.FileAccessMode,Windows.Storage,ContentType=WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapDecoder,Windows.Graphics.Imaging,ContentType=WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapPixelFormat,Windows.Graphics.Imaging,ContentType=WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapAlphaMode,Windows.Graphics.Imaging,ContentType=WindowsRuntime]
$null = [Windows.Graphics.Imaging.ExifOrientationMode,Windows.Graphics.Imaging,ContentType=WindowsRuntime]
$null = [Windows.Graphics.Imaging.ColorManagementMode,Windows.Graphics.Imaging,ContentType=WindowsRuntime]
$null = [Windows.Media.Ocr.OcrEngine,Windows.Foundation,ContentType=WindowsRuntime]
$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
} | Select-Object -First 1
function Await($operation, [Type]$type) {
    $task = $asTask.MakeGenericMethod($type).Invoke($null, @($operation))
    $task.GetAwaiter().GetResult()
}
$engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
if ($null -eq $engine) { exit 5 }
$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($env:WORKCONTEXT_OCR_FILE)) ([Windows.Storage.StorageFile])
$stream = Await ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream,Windows.Storage.Streams,ContentType=WindowsRuntime])
$decoder = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])
if ([long]$decoder.PixelWidth * $decoder.PixelHeight -gt 40000000) { exit 4 }
$scale = [Math]::Min(1.0, [Windows.Media.Ocr.OcrEngine]::MaxImageDimension / [Math]::Max($decoder.PixelWidth, $decoder.PixelHeight))
$transform = [Windows.Graphics.Imaging.BitmapTransform,Windows.Graphics.Imaging,ContentType=WindowsRuntime]::new()
$transform.ScaledWidth = [Math]::Max(1, [Math]::Floor($decoder.PixelWidth * $scale))
$transform.ScaledHeight = [Math]::Max(1, [Math]::Floor($decoder.PixelHeight * $scale))
$op = $decoder.GetSoftwareBitmapAsync([Windows.Graphics.Imaging.BitmapPixelFormat]::Bgra8, [Windows.Graphics.Imaging.BitmapAlphaMode]::Ignore, $transform, [Windows.Graphics.Imaging.ExifOrientationMode]::RespectExifOrientation, [Windows.Graphics.Imaging.ColorManagementMode]::DoNotColorManage)
$bitmap = Await $op ([Windows.Graphics.Imaging.SoftwareBitmap,Windows.Graphics.Imaging,ContentType=WindowsRuntime])
$result = Await ($engine.RecognizeAsync($bitmap)) ([Windows.Media.Ocr.OcrResult,Windows.Foundation,ContentType=WindowsRuntime])
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
[Console]::Write($result.Text.Substring(0, [Math]::Min($result.Text.Length, 262144)))
"""


def _windows_image(path, text):
    powershell = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    with tempfile.TemporaryFile(dir=".") as output:
        run = subprocess.run([str(powershell), "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", _WIN_OCR],
                             env={**os.environ, "WORKCONTEXT_OCR_FILE": str(Path(path).resolve())},
                             stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.DEVNULL,
                             creationflags=subprocess.CREATE_NO_WINDOW)
        if run.returncode:
            raise Rejected({4: "image_pixel_limit", 5: "missing:windows_ocr_language"}.get(
                run.returncode, "reader_failed:windows_ocr"))
        output.seek(0)
        text.add(output.read(_MAX_OUTPUT).decode("utf-8", "ignore"), "")


def _image(path, text):
    if os.name == "nt":
        _windows_image(path, text)
        return
    if sys.platform == "darwin":
        _native(text, "swift", str(_SWIFT), "image", path)
        return
    try:
        from PIL import Image
    except ImportError:
        raise Rejected("missing:Pillow") from None
    Image.MAX_IMAGE_PIXELS = _MAX_PIXELS
    # Pass only a bounded, single-frame raster to Tesseract (not the original).
    with Image.open(path) as image:
        if image.width * image.height > _MAX_PIXELS:
            raise Rejected("image_pixel_limit")
        image.thumbnail((3500, 3500))
        image.convert("RGB").save("ocr.png")
    exe = _program("tesseract")
    langs = subprocess.run([exe, "--list-langs"], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                           stderr=subprocess.DEVNULL, check=True).stdout.splitlines()
    language = "eng+chi_sim" if b"chi_sim" in langs else "eng"
    _native(text, exe, "ocr.png", "stdout", "-l", language)


def _legacy_ppt(stream, size, text, depth=0):
    if depth > 64:
        raise Rejected("ppt_record_depth_limit")
    end = stream.tell() + size
    while stream.tell() + 8 <= end:
        version, record, length = struct.unpack("<HHI", stream.read(8))
        stop = stream.tell() + length
        if stop > end:
            raise Rejected("invalid_ppt_record")
        if version & 15 == 15:
            _legacy_ppt(stream, length, text, depth + 1)
        elif record in (4000, 4008, 4026):
            encoding = "cp1252" if record == 4008 else "utf-16-le"
            text.add(stream.read(min(length, 2 * _MAX_OUTPUT)).decode(encoding, "replace"))
        stream.seek(stop)


def _legacy(path, suffix, text):
    if suffix not in (".doc", ".xls", ".ppt"):
        try:
            import olefile
        except ImportError:
            raise Rejected("missing:olefile") from None
        with olefile.OleFileIO(path) as doc:
            if doc.exists("WordDocument"):
                suffix = ".doc"
            elif doc.exists("Workbook") or doc.exists("Book"):
                suffix = ".xls"
            elif doc.exists("PowerPoint Document"):
                suffix = ".ppt"
            else:
                return False
    if suffix == ".doc":
        try:
            from legacy_doc import extract_text
        except ImportError:
            if sys.platform == "darwin":
                _native(text, "textutil", "-convert", "txt", "-format", "doc", "-stdout", path)
            else:
                _native(text, "antiword", path)
        else:
            text.add(extract_text(Path(path).read_bytes()).text)
    elif suffix == ".xls":
        try:
            import xlrd
        except ImportError:
            raise Rejected("missing:xlrd") from None
        book = xlrd.open_workbook(path, on_demand=True, ragged_rows=True)
        try:
            for sheet in book.sheet_names():
                text.add(sheet)
                for row in book.sheet_by_name(sheet).get_rows():
                    for cell in row:
                        if cell.value != "":
                            text.add(str(cell.value), "\t")
                    text.add("\n", "")
                book.unload_sheet(sheet)
        finally:
            book.release_resources()
    else:
        try:
            import olefile
        except ImportError:
            raise Rejected("missing:olefile") from None
        with olefile.OleFileIO(path) as doc, doc.openstream("PowerPoint Document") as stream:
            _legacy_ppt(stream, doc.get_size("PowerPoint Document"), text)
    return True


def _extract(path, name, text):
    with open(path, "rb") as file:
        magic = file.read(1024)
    if magic.startswith((b"PK\x03\x04", b"PK\x05\x06")):
        return _office(path, text)
    if b"%PDF-" in magic[:1024]:
        _pdf(path, text)
    elif magic.startswith(_OLE):
        return _legacy(path, Path(name).suffix.lower(), text)
    elif (magic.startswith((b"\xff\xd8\xff", b"\x89PNG\r\n\x1a\n", b"GIF87a", b"GIF89a", b"II*\0", b"MM\0*", b"BM"))
          or (magic.startswith(b"RIFF") and magic[8:12] == b"WEBP")
          or (magic[4:8] == b"ftyp" and magic[8:12] in (b"heic", b"heix", b"mif1", b"avif"))):
        _image(path, text)
    elif Path(name).suffix.lower() in (".txt", ".md", ".csv", ".tsv", ".log", ".json"):
        with open(path, "rb") as stream:
            raw = stream.read(_MAX_OUTPUT + 4)
        encoding = "utf-16" if raw.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-8-sig"
        if encoding != "utf-16" and b"\x00" in raw:
            return False
        text.add(raw.decode(encoding, "replace"), "")
    else:
        return False
    return True


def _windows_job():
    # Attach this worker before starting DWS or any parser. Descendants inherit
    # the job; killing the worker closes its sole job handle and reaps them all.
    import ctypes as c
    from ctypes import wintypes as w

    class Basic(c.Structure):
        _fields_ = [("process_time", c.c_int64), ("job_time", c.c_int64), ("flags", w.DWORD),
                    ("min_ws", c.c_size_t), ("max_ws", c.c_size_t), ("active", w.DWORD),
                    ("affinity", c.c_size_t), ("priority", w.DWORD), ("scheduling", w.DWORD)]

    class Extended(c.Structure):
        _fields_ = [("basic", Basic), ("io", c.c_uint64 * 6), ("process_memory", c.c_size_t),
                    ("job_memory", c.c_size_t), ("peak_process", c.c_size_t), ("peak_job", c.c_size_t)]

    kernel = c.WinDLL("kernel32", use_last_error=True)
    kernel.CreateJobObjectW.restype = w.HANDLE
    kernel.SetInformationJobObject.argtypes = (w.HANDLE, c.c_int, c.c_void_p, w.DWORD)
    kernel.AssignProcessToJobObject.argtypes = (w.HANDLE, w.HANDLE)
    job = kernel.CreateJobObjectW(None, None)  # non-inheritable handle, held until exit
    limits = Extended()
    limits.basic.flags = 0x2000 | 0x200  # KILL_ON_JOB_CLOSE | JOB_MEMORY
    limits.job_memory = 1536 << 20
    if not (job and kernel.SetInformationJobObject(job, 9, c.byref(limits), c.sizeof(limits))
            and kernel.AssignProcessToJobObject(job, w.HANDLE(-1))):
        raise Rejected("windows_job_unavailable")


def _limit(which, value):
    import resource

    soft, hard = resource.getrlimit(which)
    value = min(value, soft) if soft != resource.RLIM_INFINITY else value
    resource.setrlimit(which, (value, hard))


def _main(args):
    if os.name == "nt":
        try:
            _windows_job()
        except Rejected as exc:
            print(str(exc), file=sys.stderr)
            return 2
        if args[0] == "limit":
            # CREATE_NO_WINDOW children must receive the wrapper's redirected handles
            # explicitly; a successful DWS download without its receipt is unusable.
            return subprocess.call(args[2:], stdin=subprocess.DEVNULL, stdout=sys.stdout.buffer,
                                   stderr=sys.stderr.buffer, creationflags=subprocess.CREATE_NO_WINDOW)
    else:
        import resource

        _limit(resource.RLIMIT_CORE, 0)
        if args[0] == "limit":
            _limit(resource.RLIMIT_FSIZE, int(args[1]))
            os.execv(args[2], args[2:])
        _limit(resource.RLIMIT_FSIZE, 64 << 20)
        _limit(resource.RLIMIT_CPU, 45)
        if sys.platform != "darwin":
            _limit(resource.RLIMIT_AS, 1 << 30)
    text = _Text()
    try:
        if not _extract(args[1], args[2], text):
            return 3
    except _Full:
        pass
    except Rejected as exc:
        print(str(exc), file=sys.stderr)
        return 2
    except Exception as exc:
        print(type(exc).__name__, file=sys.stderr)
        return 2
    sys.stdout.buffer.write(text.data.getbuffer())
    return 0


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
