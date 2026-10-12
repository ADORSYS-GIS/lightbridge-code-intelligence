import yaml

with open('/home/christian/ai/ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml', 'r') as f:
    lines = f.readlines()

ranges = [(125, 144), (221, 245)]
new_lines = []
for i, line in enumerate(lines):
    line_num = i + 1
    if any(start <= line_num <= end for start, end in ranges):
        continue
    new_lines.append(line)

with open('/home/christian/ai/ai-helm-values/environments/prod/values/lightbridge-code-intelligence.yaml', 'w') as f:
    f.writelines(new_lines)
