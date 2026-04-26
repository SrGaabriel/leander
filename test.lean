def hello : String := "a"

theorem hello_world : hello = "a" := by
  have h : hello = "a" := rfl
  rfl