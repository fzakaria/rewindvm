# gdb's side of `rewind where`, sourced after the symbols are loaded.
#
# Prints one line, MARKER and then a JSON object, for rewind to read out of
# whatever else gdb prints. With $rewind_tid set, gdb is connected to a fork
# of the run, and the object lists that thread's frames, innermost first.
# With $rewind_address set, gdb has only the symbol files, and the object
# names the function, source line and file the address is in.

import json
import re

import gdb

MARKER = "rewind-where: "

# The most frames listed: an unwinder that has lost its way can go on far
# past the program's real stack.
MAX_FRAMES = 64

# `info symbol` names the function an address is in, its offset into it
# when not 0, and, with several files loaded, the file.
INFO_SYMBOL = re.compile(
    r"^(?P<name>.+?)(?: \+ (?P<offset>\d+))? in section \S+(?: of (?P<object>.+))?$"
)


def answer(value):
    print(MARKER + json.dumps(value), flush=True)


def info_symbol(pc):
    """The match of `info symbol` for pc, or None when no symbol has it."""
    text = gdb.execute("info symbol {:#x}".format(pc), to_string=True).strip()
    return INFO_SYMBOL.match(text)


def object_of(pc):
    """The program or library pc is in, when gdb knows."""
    finder = getattr(gdb.current_progspace(), "objfile_for_address", None)
    if finder is not None:
        objfile = finder(pc)
        if objfile is not None:
            return objfile.filename
    symbol = info_symbol(pc)
    if symbol is None:
        return None
    return symbol.group("object")


def source_of(sal):
    """The file a source line is in, as the DWARF names it and as it was
    found here, and the line."""
    if sal.symtab is None or sal.line == 0:
        return None, None, None
    return sal.symtab.filename, sal.symtab.fullname(), sal.line


def frame_entry(frame):
    pc = frame.pc()
    filename, fullname, line = source_of(frame.find_sal())
    function = frame.name()
    if function is None:
        symbol = info_symbol(pc)
        function = symbol.group("name") if symbol is not None else None
    return {
        "level": frame.level(),
        "function": function,
        "file": filename,
        "fullname": fullname,
        "line": line,
        "pc": "{:#x}".format(pc),
        "object": object_of(pc),
    }


def frames_of(tid):
    """Thread tid's frames, innermost first."""
    thread = next(
        (t for t in gdb.selected_inferior().threads() if t.ptid[1] == tid), None
    )
    if thread is None:
        return {"error": "gdb sees no thread {}".format(tid)}
    thread.switch()

    # A return address of 0 is where an unwinder without call frame
    # information for the program's entry point runs off its stack.
    frames = []
    frame = gdb.newest_frame()
    while frame is not None and frame.pc() != 0 and len(frames) < MAX_FRAMES:
        frames.append(frame_entry(frame))
        try:
            frame = frame.older()
        except gdb.error:
            break
    return {"frames": frames}


def place_of(address):
    """The function, offset, source line and file of an address."""
    symbol = info_symbol(address)
    filename, fullname, line = source_of(gdb.find_pc_line(address))
    return {
        "address": "{:#x}".format(address),
        "function": symbol.group("name") if symbol is not None else None,
        "offset": int(symbol.group("offset") or 0) if symbol is not None else None,
        "file": filename,
        "fullname": fullname,
        "line": line,
        "object": object_of(address),
    }


def main():
    tid = gdb.convenience_variable("rewind_tid")
    address = gdb.convenience_variable("rewind_address")
    try:
        if tid is not None:
            answer(frames_of(int(tid)))
        elif address is not None:
            answer(place_of(int(address)))
        else:
            answer({"error": "neither $rewind_tid nor $rewind_address is set"})
    except gdb.error as e:
        answer({"error": str(e)})


main()
