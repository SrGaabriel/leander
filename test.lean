def createFile (path : System.FilePath) (content : String) : IO Unit := do
  let handle ← IO.FS.Handle.mk path IO.FS.Mode.write
  handle.putStr content
  handle.flush

def test_string : String := s!"Hello, World!"

/-- A test of a theorem -/
theorem add_zero (n : Nat) : n + 0 = n := by
  induction n with
  | zero => rfl
  | succ n ih =>
      have h₁ : Nat.succ n + 0 = Nat.succ (n + 0) := by
        rfl
      have h₂ : Nat.succ (n + 0) = Nat.succ n := by
        apply congrArg Nat.succ
        rfl
      have h₃ : Nat.succ n + 0 = Nat.succ n := by
        exact Eq.trans h₁ h₂
      exact h₃
