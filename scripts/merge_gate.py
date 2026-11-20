"""Fail-closed autonomous merge preflight. Requires an authenticated gh CLI."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SESSION = r'[A-Za-z0-9._:-]+'
AUTHOR = re.compile(r'^Author-Session: (' + SESSION + r')[ \t]*$', re.MULTILINE)
MARKER = re.compile(r'<!-- llmux-verify:v1 sha=([0-9a-f]{40}) verdict=(pass|fail) reviewer=('
                    + SESSION + r') -->')


def reviewer_login(review):
    return (review.get('user') or {}).get('login', '[deleted]').lower().removesuffix('[bot]')


def evaluate(data, required, policy):
    """Return every failed condition, without IO or mutation."""
    errors = []
    pr = data['pr']
    sha = pr['head']['sha']
    if pr['state'] != 'open':
        errors.append('PR is not open.')
    if pr['draft']:
        errors.append('PR is a draft.')
    if pr['base']['ref'] != data['default_branch']:
        errors.append('PR base is not the default branch.')
    if pr['mergeable'] is not True:
        errors.append('GitHub has not reported the PR mergeable (conflict or unknown).')

    checks = [c for c in data['checks'] if c['head_sha'] == sha]
    for context in required:
        if not any(c['name'] == context and c['status'] == 'completed'
                   and c['conclusion'] == 'success' for c in checks):
            errors.append(f'Required check {context!r} has no completed successful run on {sha}.')
    for check in checks:
        if check['conclusion'] in ('failure', 'cancelled', 'timed_out', 'action_required', 'startup_failure', 'stale'):
            errors.append(f'Check {check["name"]!r} failed: {check["conclusion"]}.')
    if any(t['isResolved'] is not True for t in data['threads']):
        errors.append('Unresolved review threads remain.')

    reviews = [r for r in data['reviews'] if r['state'] != 'PENDING']
    def unavailable(review):
        login = reviewer_login(review)
        return any(m.lower() in (review.get('body') or '').lower()
                   for m in policy['unable_to_review_markers'].get(login, []))

    if not any(reviewer_login(r) == 'devin-ai-integration'
               and r.get('commit_id') == sha and not unavailable(r) for r in reviews):
        errors.append('Devin Review has not submitted a review on the head SHA.')
    latest = {}
    for review in sorted(reviews, key=lambda r: r['id']):
        latest[reviewer_login(review)] = review
    for login, review in latest.items():
        if unavailable(review):
            continue
        if review['state'] == 'CHANGES_REQUESTED' or any(
                m.lower() in (review.get('body') or '').lower()
                for m in policy['blocking_markers'].get(login, [])):
            errors.append(f'Latest review by {login} is blocking.')

    authors = AUTHOR.findall(pr.get('body') or '')
    if len(authors) != 1:
        errors.append('PR body must contain exactly one Author-Session: <id> line.')
    markers = [m.groups() for c in sorted(data['comments'], key=lambda c: c['id'])
               for m in MARKER.finditer(c.get('body') or '') if m.group(1) == sha]
    if not markers:
        errors.append('No independent verification marker for the head SHA.')
    else:
        _, verdict, reviewer = markers[-1]
        if verdict != 'pass':
            errors.append('Latest verification verdict for the head SHA is fail.')
        if len(authors) == 1 and reviewer == authors[0]:
            errors.append('Verification reviewer is the author session (self-verification).')
    return errors


def gh(*args):
    result = subprocess.run(['gh', 'api', *args], check=True, capture_output=True, text=True)
    value = json.loads(result.stdout)
    if isinstance(value, dict) and value.get('errors'):
        raise ValueError('GitHub GraphQL returned errors')
    return value


def pages(endpoint, key=None):
    items = []
    page = 1
    while True:
        value = gh(f'{endpoint}{"&" if "?" in endpoint else "?"}per_page=100&page={page}')
        batch = value[key] if key else value
        items.extend(batch)
        if len(batch) < 100:
            return items
        page += 1


def fetch(number):
    repo = gh('repos/{owner}/{repo}')
    owner, name = repo['full_name'].split('/')
    endpoint = f'repos/{owner}/{name}'
    pr = gh(f'{endpoint}/pulls/{number}')
    sha = pr['head']['sha']
    threads = []
    cursor = None
    while True:
        query = '''query($owner:String!,$name:String!,$number:Int!,$cursor:String) {
          repository(owner:$owner,name:$name) { pullRequest(number:$number) {
            reviewThreads(first:100,after:$cursor) {
              nodes { isResolved } pageInfo { hasNextPage endCursor }
            }
          } }
        }'''
        args = ['graphql', '-f', f'query={query}', '-f', f'owner={owner}',
                '-f', f'name={name}', '-F', f'number={number}']
        if cursor is not None:
            args += ['-f', f'cursor={cursor}']
        connection = gh(*args)['data']['repository']['pullRequest']['reviewThreads']
        threads.extend(connection['nodes'])
        if not connection['pageInfo']['hasNextPage']:
            break
        cursor = connection['pageInfo']['endCursor']
        if not cursor:
            raise ValueError('Missing review thread pagination cursor')
    data = {'pr': pr, 'default_branch': repo['default_branch'], 'threads': threads,
            'checks': pages(f'{endpoint}/commits/{sha}/check-runs?filter=all', 'check_runs'),
            'reviews': pages(f'{endpoint}/pulls/{number}/reviews'),
            'comments': pages(f'{endpoint}/issues/{number}/comments')}
    if gh(f'{endpoint}/pulls/{number}')['head']['sha'] != sha:
        raise ValueError('PR head changed while fetching; rerun the gate')
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('number', type=int)
    args = parser.parse_args()
    try:
        ruleset = json.loads((ROOT / '.github/rulesets/main.json').read_text())
        required = next(r['parameters']['required_status_checks'] for r in ruleset['rules']
                        if r['type'] == 'required_status_checks')
        if not required:
            raise ValueError('Ruleset contains no required checks')
        policy = json.loads((ROOT / '.github/merge-policy.json').read_text())
        data = fetch(args.number)
        errors = evaluate(data, [c['context'] for c in required], policy)
    except (OSError, ValueError, KeyError, TypeError, StopIteration, subprocess.SubprocessError) as exc:
        print(f'FAIL: Unable to evaluate merge gate: {exc}', file=sys.stderr)
        return 1
    for error in errors:
        print(f'FAIL: {error}')
    if not errors:
        print(f'Merge gate passed for PR #{args.number} at {data["pr"]["head"]["sha"]}.')
    return 1 if errors else 0


if __name__ == '__main__':
    sys.exit(main())
