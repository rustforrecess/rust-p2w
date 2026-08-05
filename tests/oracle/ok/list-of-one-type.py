# A list built up one append at a time, then read back.
#
# The element type is never written down anywhere. It has to come from the
# literal and survive `append`, indexing and iteration.

scores = [10, 20, 30]
scores.append(40)

best = scores[0]
for s in scores:
    if s > best:
        best = s

print(best)
