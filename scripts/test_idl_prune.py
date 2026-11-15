import unittest

from idl_prune import prune_unresolved_types


def struct(name, *fields):
    return {"name": name, "type": {"kind": "struct", "fields": [{"name": n, "type": t} for n, t in fields]}}


class PruneUnresolvedTypes(unittest.TestCase):
    def test_leaves_a_resolvable_idl_untouched(self):
        idl = {"types": [struct("A", ("x", "u8")), struct("B", ("a", {"defined": "A"}))], "instructions": []}
        out, removed = prune_unresolved_types(idl)
        self.assertEqual(removed, [])
        self.assertEqual([t["name"] for t in out["types"]], ["A", "B"])

    def test_drops_unused_types_over_an_undefined_generic_and_cascades(self):
        idl = {
            "types": [
                struct("Keep", ("x", "u8")),
                struct("Msg", ("keys", {"defined": "SmallVec<u8,Pubkey>"})),
                struct("Wrapper", ("m", {"defined": "Msg"})),
            ],
            "instructions": [{"name": "go", "args": [{"name": "k", "type": {"defined": "Keep"}}]}],
        }
        out, removed = prune_unresolved_types(idl)
        self.assertEqual(sorted(removed), ["Msg", "Wrapper"])
        self.assertEqual([t["name"] for t in out["types"]], ["Keep"])

    def test_fails_loudly_when_an_instruction_needs_a_pruned_type(self):
        idl = {
            "types": [struct("Msg", ("keys", {"defined": "SmallVec<u8,Pubkey>"}))],
            "instructions": [{"name": "go", "args": [{"name": "m", "type": {"defined": "Msg"}}]}],
        }
        with self.assertRaises(ValueError) as ctx:
            prune_unresolved_types(idl)
        self.assertIn("Msg", str(ctx.exception))

    def test_fails_when_a_named_account_type_needs_a_pruned_type(self):
        idl = {
            "types": [struct("Vault", ("keys", {"defined": "SmallVec<u8,Pubkey>"}))],
            "accounts": [{"name": "Vault", "discriminator": [1, 2, 3, 4, 5, 6, 7, 8]}],
            "instructions": [],
        }
        with self.assertRaises(ValueError) as ctx:
            prune_unresolved_types(idl)
        self.assertIn("Vault", str(ctx.exception))

    def test_fails_when_a_named_event_type_needs_a_pruned_type(self):
        idl = {
            "types": [struct("Moved", ("keys", {"defined": "SmallVec<u8,Pubkey>"}))],
            "events": [{"name": "Moved", "discriminator": [1, 2, 3, 4, 5, 6, 7, 8]}],
            "instructions": [],
        }
        with self.assertRaises(ValueError) as ctx:
            prune_unresolved_types(idl)
        self.assertIn("Moved", str(ctx.exception))

    def test_still_prunes_a_type_no_account_or_event_names(self):
        idl = {
            "types": [struct("Vault", ("x", "u8")), struct("Unused", ("k", {"defined": "Missing"}))],
            "accounts": [{"name": "Vault", "discriminator": [1, 2, 3, 4, 5, 6, 7, 8]}],
            "instructions": [],
        }
        _, removed = prune_unresolved_types(idl)
        self.assertEqual(removed, ["Unused"])

    def test_understands_both_defined_spellings(self):
        idl = {"types": [struct("Msg", ("k", {"defined": {"name": "Missing"}}))], "instructions": []}
        _, removed = prune_unresolved_types(idl)
        self.assertEqual(removed, ["Msg"])

    def test_codama_idls_without_types_are_a_noop(self):
        idl = {"program": {"name": "p"}}
        self.assertEqual(prune_unresolved_types(idl), (idl, []))


if __name__ == "__main__":
    unittest.main()
