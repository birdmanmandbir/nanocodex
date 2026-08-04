import csv
import json
import random
import sys

rows = list(csv.DictReader(open(sys.argv[1], newline="", encoding="utf-8")))
limit = int(sys.argv[2])
rng = random.Random(0)

# GPQAEval's published default evaluates four independently shuffled copies of
# every Diamond example. A finite import limit is a smoke-only prefix of that
# same deterministic stream.
for index, row in enumerate(rows * 4):
    if index >= limit:
        break
    permutation = rng.sample(range(4), 4)
    choices = [
        row["Correct Answer"],
        row["Incorrect Answer 1"],
        row["Incorrect Answer 2"],
        row["Incorrect Answer 3"],
    ]
    choices = [choices[index] for index in permutation]
    print(
        json.dumps(
            {
                "question": row["Question"],
                "choices": choices,
                "correct_answer": "ABCD"[choices.index(row["Correct Answer"])],
            }
        )
    )
