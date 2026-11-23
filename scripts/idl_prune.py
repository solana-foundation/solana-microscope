"""Drop Anchor IDL type definitions that reference types the IDL does not define."""


def _refs(node, out):
    if isinstance(node, dict):
        defined = node.get("defined")
        if defined is not None:
            out.add(defined if isinstance(defined, str) else defined.get("name"))
        for value in node.values():
            _refs(value, out)
    elif isinstance(node, list):
        for value in node:
            _refs(value, out)
    return out


def prune_unresolved_types(idl):
    """Remove types with unresolvable references (repeating until stable) and return (idl, removed names).

    Raises ValueError when an instruction, event or account still depends on a removed type.
    """
    types = idl.get("types") or []
    kept = list(types)
    # pre-0.30 Anchor defines accounts and events with an inline "type"; those names are defined too
    inline = {
        entry["name"]
        for section in ("accounts", "events")
        for entry in idl.get(section) or []
        if "name" in entry and "type" in entry
    }
    while True:
        names = {t["name"] for t in kept} | inline
        next_kept = [t for t in kept if all(r in names for r in _refs(t, set()))]
        if len(next_kept) == len(kept):
            break
        kept = next_kept
    removed = [t["name"] for t in types if t not in kept]
    if not removed:
        return idl, []
    still_used = set()
    for section in ("instructions", "events", "accounts"):
        _refs(idl.get(section) or [], still_used)
    for section in ("events", "accounts"):
        still_used.update(entry["name"] for entry in idl.get(section) or [] if "name" in entry)
    needed = sorted(set(removed) & still_used)
    if needed:
        raise ValueError(
            "IDL type(s) used by instructions, events or accounts reference types the IDL does not define: "
            + ", ".join(needed)
        )
    idl["types"] = kept
    return idl, removed
