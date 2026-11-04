# gdb's side of `rewind where`, sourced after the symbols are loaded.
#
# Prints one line, MARKER and then a JSON object, for rewind to read out of
# whatever else gdb prints. With $rewind_tid set, gdb is connected to a fork
# of the run, and the object lists that thread's frames, innermost first,
# each with its arguments and locals.
# With $rewind_expressions also set, to a file of JSON listing expressions,
# each frame carries what each evaluates to there.
# With $rewind_threads set, to a file of JSON with thread ids and
# addresses, the object lists each thread's frames, without their
# variables, and the place of each address.
# With $rewind_address set, gdb has only the symbol files, and the object
# names the function or variable, source line and file the address is in.

import json
import re

import gdb

MARKER = "rewind-where: "

# The most frames listed: an unwinder that has lost its way can go on far
# past the program's real stack.
MAX_FRAMES = 64

# The most variables listed for a frame, and how a value prints: no deeper
# than two levels of structs, the first elements of an array or string,
# and no longer than a line a person reads, cut short past that.
MAX_VARIABLES = 64
MAX_DEPTH = 2
MAX_ELEMENTS = 32
MAX_VALUE = 400
CUT = "..."

# The lines around a frame's line whose expressions are evaluated, above
# it within its function and below it, and the most expressions a frame
# lists.
LINES_ABOVE = 8
LINES_BELOW = 2
MAX_EXPRESSIONS = 64

# The most bytes a value can be watched for: the CPU's four debug address
# registers, 8 bytes each (rewind-core's accesses::MAX_BYTES).
MAX_WATCHED = 32

# A chain of names in source: a name, then fields through `.` or `->` and
# subscripts by a name or a number. Each prefix that ends in a name is an
# expression of its own, `p` and `p->queue` in `p->queue->completed`.
NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
LINK = re.compile(r"\s*(->|\.)\s*([A-Za-z_][A-Za-z0-9_]*)|\s*\[\s*([A-Za-z0-9_]+)\s*\]")

# Words that are never a value: C's and C++'s keywords and types.
KEYWORDS = frozenset(
    """auto bool break case char const continue default do double else enum
    extern float for goto if inline int long register restrict return short
    signed sizeof static struct switch typedef union unsigned void volatile
    while NULL true false nullptr this class new delete template typename
    namespace using public private protected virtual operator""".split()
)

# Comments, and string and character literals, blanked out of source so
# their words are not taken for expressions. A block comment can span
# lines.
BLANKED = re.compile(
    r'//[^\n]*|/\*.*?(\*/|$)|"(\\.|[^"\\\n])*"|\'(\\.|[^\'\\\n])*\'', re.DOTALL
)

# `info address` says where a variable is: in a register, or for a
# location list, in which register over each range of instructions.
IN_REGISTER = re.compile(r"in \$(\w+)")
RANGE = re.compile(r"Range (0x[0-9a-f]+)-(0x[0-9a-f]+): (.*)")

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


def value_text(symbol, frame):
    """symbol's value in frame on one line, as gdb prints it, or why gdb
    cannot read it."""
    try:
        text = symbol.value(frame).format_string(
            max_depth=MAX_DEPTH,
            max_elements=MAX_ELEMENTS,
            pretty_structs=False,
            pretty_arrays=False,
            styling=False,
        )
    except gdb.error as e:
        text = "<error: {}>".format(e)
    if len(text) > MAX_VALUE:
        text = text[: MAX_VALUE - len(CUT)] + CUT
    return text


def value_class(type_):
    """How the bytes of a value of type_ print: as a pointer, a signed or
    unsigned integer, a bool, a floating-point number, or as bytes."""
    t = type_.strip_typedefs()
    if t.code == gdb.TYPE_CODE_PTR:
        return "pointer"
    if t.code == gdb.TYPE_CODE_BOOL:
        return "bool"
    if t.code == gdb.TYPE_CODE_FLT:
        return "float"
    if t.code in (gdb.TYPE_CODE_INT, gdb.TYPE_CODE_CHAR, gdb.TYPE_CODE_ENUM):
        return "signed" if getattr(t, "is_signed", True) else "unsigned"
    return "bytes"


def register_of(name, pc):
    """The register variable name is in at pc, as `info address` says, or
    None."""
    try:
        text = gdb.execute("info address " + name, to_string=True)
    except gdb.error:
        return None
    for line in text.splitlines():
        found = RANGE.search(line)
        if found is None:
            continue
        if int(found.group(1), 16) <= pc < int(found.group(2), 16):
            register = IN_REGISTER.search(found.group(3))
            return register.group(1) if register else None
    if "multi-location" in text:
        return None
    register = IN_REGISTER.search(text)
    return register.group(1) if register else None


def location_of(value, name=None, pc=None):
    """Where value is, and whether a watch can be put on it: in memory at
    an address, of a size and a class of type; in a register; optimized
    out; or in memory that does not read."""
    try:
        if value.is_optimized_out:
            return {"kind": "optimized-out"}
        size = value.type.strip_typedefs().sizeof
        address = value.address
        if address is None:
            register = register_of(name, pc) if name is not None else None
            return {"kind": "register", "register": register}
        value.fetch_lazy()
    except gdb.MemoryError:
        return {"kind": "unreadable"}
    except gdb.error:
        return {"kind": "unreadable"}
    return {
        "kind": "memory" if size <= MAX_WATCHED else "too-large",
        "address": "{:#x}".format(int(address)),
        "size": size,
        "class": value_class(value.type),
        "type": str(value.type),
    }


def variables_of(frame):
    """frame's arguments and locals, as `info args` and `info locals` list
    them: the blocks from the one around the frame's instruction out to
    the function's own, innermost first. A frame without DWARF has none."""
    try:
        block = frame.block()
    except (RuntimeError, gdb.error):
        return [], []
    arguments, locals_ = [], []
    while block is not None:
        for symbol in block:
            if len(arguments) + len(locals_) >= MAX_VARIABLES:
                break
            if symbol.is_argument:
                arguments.append(symbol)
            elif symbol.is_variable:
                locals_.append(symbol)
        if block.function is not None:
            break
        block = block.superblock

    def entries(symbols):
        out = []
        for s in symbols:
            entry = {"name": s.name, "value": value_text(s, frame)}
            try:
                entry["location"] = location_of(s.value(frame), s.name, frame.pc())
            except gdb.error:
                entry["location"] = {"kind": "unreadable"}
            out.append(entry)
        return out

    return entries(arguments), entries(locals_)


def blanked(text):
    """text with its comments and literals made spaces, its newlines
    kept."""
    return BLANKED.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), text)


def chains(text, blank):
    """Each expression in a line of source, blank the line blanked, a name and the fields and
    subscripts after it, by every prefix ending in a name: the expression,
    where it starts, and where its last name starts and ends, in
    characters. A name called as a function is none."""
    found = []
    at = 0
    while True:
        start = NAME.search(blank, at)
        if start is None:
            return found
        at = start.end()
        if start.group(0) in KEYWORDS or (start.start() > 0 and blank[start.start() - 1].isdigit()):
            continue
        prefixes = [(start.start(), start.end())]
        end = start.end()
        while True:
            link = LINK.match(blank, end)
            if link is None:
                break
            end = link.end()
            if link.group(2) is not None:
                prefixes.append((link.start(2), end))
        at = end
        if blank[end:].lstrip().startswith("("):
            prefixes.pop()
        for name_start, name_end in prefixes:
            found.append(
                {
                    "text": re.sub(r"\s+", "", text[start.start() : name_end]),
                    "start": start.start(),
                    "name_start": name_start,
                    "end": name_end,
                }
            )


def source_lines(fullname, line, function):
    """The lines of fullname around line, within function's, by number,
    each as it reads and blanked. Blanking starts at the function's first
    line, so a comment begun above the lines is known."""
    try:
        with open(fullname, errors="replace") as f:
            text = f.read().splitlines()
    except OSError:
        return []
    first = line - LINES_ABOVE
    begins = max(first, 1)
    if function is not None and function.line:
        first = max(first, function.line)
        begins = min(function.line, begins)
    first = max(first, 1)
    last = min(line + LINES_BELOW, len(text))
    blank = blanked("\n".join(text[begins - 1 : last])).split("\n")
    return [(n, text[n - 1], blank[n - begins]) for n in range(first, last + 1)]


def evaluate(text, frame):
    """What expression text is in frame: its value as gdb prints it and
    its location."""
    frame.select()
    try:
        value = gdb.parse_and_eval(text)
    except gdb.error:
        return None
    entry = {"text": text}
    entry["location"] = location_of(value, text, frame.pc())
    try:
        entry["value"] = value.format_string(
            max_depth=MAX_DEPTH,
            max_elements=MAX_ELEMENTS,
            pretty_structs=False,
            pretty_arrays=False,
            styling=False,
        )[:MAX_VALUE]
    except gdb.error as e:
        entry["value"] = "<error: {}>".format(e)
    return entry


def expressions_of(frame, fullname, line):
    """The expressions on the lines around the frame's line, each with
    what it evaluates to in the frame. A name gdb does not know, such as a
    type or a macro, is left out."""
    if fullname is None or line is None:
        return []
    try:
        function = frame.function()
    except RuntimeError:
        function = None
    known = {}
    out = []
    for number, text, blank in source_lines(fullname, line, function):
        for chain in chains(text, blank):
            if len(out) >= MAX_EXPRESSIONS:
                return out
            if chain["text"] not in known:
                known[chain["text"]] = evaluate(chain["text"], frame)
            evaluated = known[chain["text"]]
            if evaluated is None:
                continue
            entry = dict(evaluated)
            entry.update(
                line=number,
                start=chain["start"],
                name_start=chain["name_start"],
                end=chain["end"],
            )
            out.append(entry)
    return out


def frame_entry(frame, asked=(), bare=False):
    """frame as rewind reads it: where it is, and unless bare, its
    variables, the expressions on the lines around its line and those in
    asked, each with what it is there."""
    pc = frame.pc()
    filename, fullname, line = source_of(frame.find_sal())
    function = frame.name()
    if function is None:
        symbol = info_symbol(pc)
        function = symbol.group("name") if symbol is not None else None
    if bare:
        arguments, locals_, expressions, evaluated = [], [], [], []
    else:
        arguments, locals_ = variables_of(frame)
        expressions = expressions_of(frame, fullname, line)
        evaluated = [evaluate(text, frame) for text in asked]
    return {
        "level": frame.level(),
        "function": function,
        "file": filename,
        "fullname": fullname,
        "line": line,
        "pc": "{:#x}".format(pc),
        "object": object_of(pc),
        "arguments": arguments,
        "locals": locals_,
        "expressions": expressions,
        "evaluated": evaluated,
    }


def frames_of(tid, asked=(), bare=False):
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
        frames.append(frame_entry(frame, asked, bare))
        try:
            frame = frame.older()
        except gdb.error:
            break
    return {"frames": frames}


def variable_named(name):
    """The global or static variable called name, or None."""
    for lookup in (gdb.lookup_global_symbol, gdb.lookup_static_symbol):
        found = lookup(name)
        if found is not None and found.is_variable and found.symtab is not None:
            return found
    return None


def place_of(address):
    """The function or variable, offset, source line and file of an
    address. A variable's line is where it is defined."""
    symbol = info_symbol(address)
    filename, fullname, line = source_of(gdb.find_pc_line(address))
    variable = None
    if symbol is not None and line is None:
        variable = variable_named(symbol.group("name"))
    if variable is not None:
        filename, fullname, line = (
            variable.symtab.filename,
            variable.symtab.fullname(),
            variable.line,
        )
    return {
        "address": "{:#x}".format(address),
        "function": symbol.group("name") if symbol is not None else None,
        "offset": int(symbol.group("offset") or 0) if symbol is not None else None,
        "file": filename,
        "fullname": fullname,
        "line": line,
        "object": object_of(address),
        "variable": variable is not None,
    }


def read_json(variable):
    """The JSON in the file a convenience variable names, or None."""
    path = gdb.convenience_variable(variable)
    if path is None:
        return None
    with open(path.string()) as f:
        return json.load(f)


def threads_and_places(asked):
    """Each thread's frames, bare, and each address's place."""
    threads = []
    for tid in asked.get("tids", []):
        walked = frames_of(int(tid), bare=True)
        walked["tid"] = int(tid)
        threads.append(walked)
    places = [place_of(int(a, 16)) for a in asked.get("addresses", [])]
    return {"threads": threads, "places": places}


def main():
    tid = gdb.convenience_variable("rewind_tid")
    address = gdb.convenience_variable("rewind_address")
    try:
        threads = read_json("rewind_threads")
        if threads is not None:
            answer(threads_and_places(threads))
        elif tid is not None:
            answer(frames_of(int(tid), read_json("rewind_expressions") or ()))
        elif address is not None:
            answer(place_of(int(address)))
        else:
            answer({"error": "neither $rewind_tid nor $rewind_address is set"})
    except gdb.error as e:
        answer({"error": str(e)})


main()
