def createFile (path : System.FilePath) (content : String) : IO Unit := do
  let handle ← IO.FS.Handle.mk path IO.FS.Mode.write
  handle.putStr content
  handle.flush

theorem add_zero (n : Nat) : n + 0 = n := by
  induction n with
  | zero =>
      have h₀ : (0 : Nat) + 0 = 0 := by
        rfl
      exact h₀
  | succ n ih =>
      have h₁ : Nat.succ n + 0 = Nat.succ (n + 0) := by
        rfl
      have h₂ : Nat.succ (n + 0) = Nat.succ n := by
        apply congrArg Nat.succ ih
      have h₃ : Nat.succ n + 0 = Nat.succ n := by
        exact Eq.trans h₁ h₂
      exact h₃
