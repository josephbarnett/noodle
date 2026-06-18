# Capture export (linked)

Client: **opencode**  ·  27 round trips  ·  4 frames

## Frame tree

- main  (ses_12eb57857ffe)  7 round trips
  - subagent_1  (ses_12eb2a1cfffe)  12 round trips
  - subagent_2  (ses_12ea76678ffe)  4 round trips
  - subagent_3  (ses_12ea7666cffe)  4 round trips

## Round trips — depth-first, linked

| seq | thread | rt | spawned_by | spawns | this_msg | stop | in | out | wire# |
|--:|---|--:|---|---|---|---|--:|--:|--:|
| 1 | main | 0 | - | - | msg_01AdhAhyDPJ6cg | tool_use | 2 | 99 | 1 |
| 2 | main | 1 | - | - | msg_01BHxdTuAogMy1 | end_turn | 602 | 343 | 2 |
| 3 | main | 2 | - | - | msg_01LJYvArpop2c5 | end_turn | 2 | 816 | 3 |
| 4 | main | 3 | - | subagent_1 | msg_01CLHDXe26FB83 | tool_use | 2 | 478 | 4 |
| 5 | subagent_1 | 0 | 004_main_rt3_spawns-1 | - | msg_01S2VzJybiuaW1 | tool_use | 2 | 182 | 5 |
| 6 | subagent_1 | 1 | - | - | msg_01MehkwRSwLAoE | tool_use | 2 | 369 | 6 |
| 7 | subagent_1 | 2 | - | - | msg_01XRo3cUMFYnBi | tool_use | 2 | 265 | 7 |
| 8 | subagent_1 | 3 | - | - | msg_01KkfqoeAcKQhD | tool_use | 2 | 273 | 8 |
| 9 | subagent_1 | 4 | - | - | msg_01JFTVfQv8zFrH | tool_use | 2 | 262 | 9 |
| 10 | subagent_1 | 5 | - | - | msg_017okTANMLT2BY | tool_use | 2 | 450 | 10 |
| 11 | subagent_1 | 6 | - | - | msg_01HxqNMy5eYowC | tool_use | 2 | 176 | 11 |
| 12 | subagent_1 | 7 | - | - | msg_01UfxMGGd7vbLm | tool_use | 2 | 176 | 12 |
| 13 | subagent_1 | 8 | - | - | msg_01F8a5Efa1e57q | tool_use | 2 | 201 | 13 |
| 14 | subagent_1 | 9 | - | - | msg_01XtE57bDdL1sX | tool_use | 2 | 262 | 14 |
| 15 | subagent_1 | 10 | - | - | msg_01FFUga7eYfsec | tool_use | 2 | 306 | 15 |
| 16 | subagent_1 | 11 | - | - | msg_01QW7fE9tjvhny | end_turn | 2 | 4195 | 16 |
| 17 | main | 4 | - | - | msg_011LqJVumyQCbN | end_turn | 2 | 1611 | 17 |
| 18 | main | 5 | - | subagent_2,subagent_3 | msg_011m46ETcsH1V1 | tool_use | 2 | 365 | 18 |
| 19 | subagent_2 | 0 | 018_main_rt5_spawns-2 | - | msg_01392tGbjW46eh | tool_use | 2 | 104 | 19 |
| 20 | subagent_2 | 1 | - | - | msg_01NrcSMtvNNR8B | tool_use | 2 | 107 | 21 |
| 21 | subagent_2 | 2 | - | - | msg_011cMZ56LXyJHW | tool_use | 2 | 110 | 22 |
| 22 | subagent_2 | 3 | - | - | msg_01A6xaJiwt2esp | end_turn | 2 | 322 | 25 |
| 23 | subagent_3 | 0 | 018_main_rt5_spawns-2 | - | msg_015Cj5QTPiLBDE | tool_use | 2 | 90 | 20 |
| 24 | subagent_3 | 1 | - | - | msg_019ya5owhegiGu | tool_use | 2 | 138 | 23 |
| 25 | subagent_3 | 2 | - | - | msg_019RijjdcKrvgc | tool_use | 2 | 157 | 24 |
| 26 | subagent_3 | 3 | - | - | msg_01NMTDKepfLUsw | end_turn | 2 | 85 | 26 |
| 27 | main | 6 | - | - | msg_01Lgr9nsRy9ZC3 | end_turn | 2 | 219 | 27 |
