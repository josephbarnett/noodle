# Capture export (linked)

Client: **claude-code**  ·  12 round trips  ·  4 frames

## Frame tree

- main  (no-agent-id)  2 round trips
  - subagent_1  (ab096c466a396adb)  4 round trips
  - subagent_2  (a78ea0e4128c236b)  3 round trips
  - subagent_3  (abe1f4c6a7b2d19d)  3 round trips

## Round trips — depth-first, linked

| seq | thread | rt | spawned_by | spawns | this_msg | stop | in | out | wire# |
|--:|---|--:|---|---|---|---|--:|--:|--:|
| 1 | main | 0 | - | subagent_1,subagent_2,subagent_3 | msg_01GniqacXBJkUs | tool_use | 11257 | 806 | 1 |
| 2 | subagent_1 | 0 | 001_main_rt0_open_spawns-3 | - | msg_01NyBM8SaUoyqK | tool_use | 7537 | 154 | 2 |
| 3 | subagent_1 | 1 | - | - | msg_01L7ekxDBXxy5t | tool_use | 4378 | 247 | 5 |
| 4 | subagent_1 | 2 | - | - | msg_01NpXeRvVLEbiX | tool_use | 2 | 1180 | 9 |
| 5 | subagent_1 | 3 | - | - | msg_01Twivd3AuMZ32 | end_turn | 5328 | 607 | 11 |
| 6 | subagent_2 | 0 | 001_main_rt0_open_spawns-3 | - | msg_01AF4Jh37xrEcf | tool_use | 7537 | 148 | 3 |
| 7 | subagent_2 | 1 | - | - | msg_01H9HKxxE5ibVx | tool_use | 4378 | 465 | 6 |
| 8 | subagent_2 | 2 | - | - | msg_01JHFoiGfajb9L | end_turn | 1469 | 285 | 7 |
| 9 | subagent_3 | 0 | 001_main_rt0_open_spawns-3 | - | msg_01Eue9hBxX2Aak | tool_use | 7537 | 159 | 4 |
| 10 | subagent_3 | 1 | - | - | msg_01DAPCTFdSmyuG | tool_use | 4378 | 1439 | 8 |
| 11 | subagent_3 | 2 | - | - | msg_017P83EohSU2tT | end_turn | 3585 | 587 | 10 |
| 12 | main | 1 | - | - | msg_01QRFpRe1CHjfv | end_turn | 406 | 1169 | 12 |
