# ri-tools

Building blocks for agent applications. Think of it like a warehouse of car parts.

The idea of ri is that you build your own agent on top of a the small core library. So to keep applications simple, we 
extract various shared logic into composable helpers in this crate. This lets each person build their own ri how they like,
by snapping together larger pieces.

Blocks should be independent in nature. Composition over inheritance.