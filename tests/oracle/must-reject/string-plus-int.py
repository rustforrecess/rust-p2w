# The classic. A student reads a number, forgets it arrived as text, and adds.
#
# WANTED: an error at line 3 that names line 1 as the reason `age` is a
# string. Reporting only "cannot add str and int at line 3" leaves them
# staring at a line where nothing is wrong.

age = "12"
next_year = age + 1
print(next_year)
