import unittest
from unittest.mock import patch

from merge_gate import evaluate, fetch, main, pages


SHA = 'a' * 40
POLICY = {'blocking_markers': {'codesolarlght': ['Changes recommended']},
          'devin_review_status_context': 'Devin Review',
          'devin_review_markers': ['<!-- devin-review-summary -->', '<!-- devin-review-session-triaged -->'],
          'verifier_logins': ['devin-ai-integration', 'soilspoon'],
          'unable_to_review_markers': {'copilot': ['quota exceeded']}}


def fixture():
    return {'pr': {'state': 'open', 'draft': False, 'base': {'ref': 'main'},
                   'mergeable': True, 'head': {'sha': SHA}, 'body': 'Author-Session: author'},
            'default_branch': 'main',
            'checks': [{'name': 'check', 'status': 'completed', 'conclusion': 'success',
                        'head_sha': SHA}],
            'threads': [{'isResolved': True}], 'statuses': [],
            'reviews': [{'id': 1, 'user': {'login': 'devin-ai-integration'},
                         'commit_id': SHA, 'state': 'COMMENTED', 'body': '<!-- devin-review-summary -->'}],
            'comments': [{'id': 1, 'user': {'login': 'devin-ai-integration[bot]'}, 'body': marker()}]}


def marker(sha=SHA, verdict='pass', reviewer='independent'):
    return f'<!-- llmux-verify:v1 sha={sha} verdict={verdict} reviewer={reviewer} -->'


class MergeGateTest(unittest.TestCase):
    def test_untrusted_status_cannot_displace_devin_status(self):
        for state in ('pending', 'failure', 'error'):
            with self.subTest(state=state):
                data = fixture()
                data['statuses'] = [
                    {'id': 1, 'context': 'Devin Review', 'state': state,
                     'creator': {'login': 'Devin-AI-Integration[bot]'}},
                    {'id': 2, 'context': 'Devin Review', 'state': 'success',
                     'creator': {'login': 'other-integration[bot]'}}]
                self.assertIn(f'Devin Review status is {state}', evaluate(data, ['check'], POLICY))
        data['statuses'][0]['state'] = 'success'
        data['statuses'][1]['state'] = 'failure'
        self.assertEqual(evaluate(data, ['check'], POLICY), [])

    def test_devin_status_success_without_review(self):
        data = fixture()
        data['reviews'] = []
        data['statuses'] = [{'id': 1, 'context': 'Devin Review', 'state': 'success',
                             'creator': {'login': 'devin-ai-integration[bot]'}}]
        self.assertEqual(evaluate(data, ['check'], POLICY), [])

    def test_latest_devin_status_overrides_review(self):
        for state in ('pending', 'failure', 'error'):
            with self.subTest(state=state):
                data = fixture()
                data['statuses'] = [
                    {'id': 2, 'context': 'Devin Review', 'state': state,
                     'creator': {'login': 'devin-ai-integration[bot]'}},
                    {'id': 1, 'context': 'Devin Review', 'state': 'success',
                     'creator': {'login': 'devin-ai-integration[bot]'}}]
                self.assertIn(f'Devin Review status is {state}', evaluate(data, ['check'], POLICY))

    def test_untrusted_or_unrelated_status_does_not_satisfy_review(self):
        for context, login in [('Devin Review', 'someone-else'),
                               ('Other Review', 'devin-ai-integration[bot]')]:
            with self.subTest(context=context, login=login):
                data = fixture()
                data['reviews'] = []
                data['statuses'] = [{'id': 1, 'context': context, 'state': 'success',
                                     'creator': {'login': login}}]
                self.assertTrue(evaluate(data, ['check'], POLICY))

    def test_empty_reply_review_does_not_clear_blocker(self):
        for body in ('', '  \r\n', None):
            with self.subTest(body=body):
                data = fixture()
                data['reviews'] += [
                    {'id': 2, 'user': {'login': 'codesolarlght'}, 'state': 'COMMENTED',
                     'body': 'Changes recommended'},
                    {'id': 3, 'user': {'login': 'codesolarlght'}, 'state': 'COMMENTED', 'body': body}]
                self.assertIn('Latest review by codesolarlght is blocking.',
                              evaluate(data, ['check'], POLICY))
        data['reviews'].append({'id': 4, 'user': {'login': 'codesolarlght'},
                                'state': 'APPROVED', 'body': ''})
        self.assertEqual(evaluate(data, ['check'], POLICY), [])

    def test_devin_review_requires_real_body_marker(self):
        for body in ('', 'Fixed in abc123', 'Devin Review', '<!-- unrelated -->'):
            with self.subTest(body=body):
                data = fixture()
                data['reviews'][0]['body'] = body
                self.assertIn('Devin Review has not submitted a review on the head SHA.',
                              evaluate(data, ['check'], POLICY))
        for body in POLICY['devin_review_markers']:
            data = fixture()
            data['reviews'][0]['body'] = body
            self.assertEqual(evaluate(data, ['check'], POLICY), [])
        data['reviews'][0]['user']['login'] = 'someone-else'
        self.assertTrue(evaluate(data, ['check'], POLICY))

    def test_queued_rerun_without_timestamps_blocks(self):
        data = fixture()
        data['checks'][0].update(id=1, started_at='2026-01-01T00:00:00Z')
        data['checks'].append(dict(data['checks'][0], id=2, started_at=None,
                                   completed_at=None, status='queued', conclusion=None))
        self.assertIn('check check still running', evaluate(data, ['check'], POLICY))

    def test_cancelled_then_success(self):
        data = fixture()
        data['checks'] += [dict(data['checks'][0], id=2, conclusion='cancelled'),
                           dict(data['checks'][0], id=3)]
        self.assertEqual(evaluate(data, ['check'], POLICY), [])

    def test_optional_check_still_running(self):
        data = fixture()
        data['checks'].append(dict(data['checks'][0], name='CodeSolar Review',
                                   status='in_progress', conclusion=None))
        self.assertIn('check CodeSolar Review still running', evaluate(data, ['check'], POLICY))

    def test_crlf_author_session(self):
        data = fixture()
        data['pr']['body'] = 'Author-Session: author\r\nMore text\r\n'
        self.assertEqual(evaluate(data, ['check'], POLICY), [])

    def test_dismissed_reviews_do_not_count(self):
        data = fixture()
        data['reviews'][0]['state'] = 'DISMISSED'
        self.assertTrue(evaluate(data, ['check'], POLICY))
        data = fixture()
        data['reviews'].append({'id': 2, 'user': {'login': 'codesolarlght'},
                                'state': 'DISMISSED', 'body': 'Changes recommended'})
        self.assertEqual(evaluate(data, ['check'], POLICY), [])

    def test_latest_check_run_wins(self):
        for name in ('check', 'extra'):
            for conclusion, passes in [('success', True), ('failure', False), (None, False)]:
                with self.subTest(name=name, conclusion=conclusion):
                    data = fixture()
                    data['checks'] += [
                        dict(data['checks'][0], name=name, id=2, started_at='2026-01-01T00:00:00Z', conclusion='failure' if passes else 'success'),
                        dict(data['checks'][0], name=name, id=3, started_at='2026-01-02T00:00:00Z', conclusion=conclusion, status='completed' if conclusion else 'in_progress')]
                    self.assertEqual(not evaluate(data, ['check'], POLICY), passes)

    def test_untrusted_verifier_is_ignored(self):
        data = fixture()
        data['comments'][0]['user']['login'] = 'outsider'
        self.assertTrue(evaluate(data, ['check'], POLICY))

    def test_pass(self):
        self.assertEqual(evaluate(fixture(), ['check'], POLICY), [])

    def test_github_bot_logins(self):
        data = fixture()
        data['reviews'][0]['user']['login'] += '[bot]'
        self.assertEqual(evaluate(data, ['check'], POLICY), [])
        data['reviews'].append({'id': 2, 'user': {'login': 'codesolarlght[bot]'},
                                'body': 'Changes recommended', 'state': 'COMMENTED'})
        self.assertTrue(evaluate(data, ['check'], POLICY))

    def test_failures(self):
        mutations = {
            'closed': lambda d: d['pr'].update(state='closed'),
            'draft': lambda d: d['pr'].update(draft=True),
            'base': lambda d: d['pr']['base'].update(ref='other'),
            'conflict': lambda d: d['pr'].update(mergeable=False),
            'unknown_mergeability': lambda d: d['pr'].update(mergeable=None),
            'missing_check': lambda d: d.update(checks=[]),
            'pending': lambda d: d['checks'][0].update(status='in_progress'),
            'failed': lambda d: d['checks'][0].update(conclusion='failure'),
            'skipped': lambda d: d['checks'][0].update(conclusion='skipped'),
            'old_check': lambda d: d['checks'][0].update(head_sha='b' * 40),
            'other_failed': lambda d: d['checks'].append(dict(d['checks'][0], name='extra', conclusion='failure')),
            'thread': lambda d: d['threads'].append({'isResolved': False}),
            'no_devin': lambda d: d.update(reviews=[]),
            'old_devin': lambda d: d['reviews'][0].update(commit_id='b' * 40),
            'changes_requested': lambda d: d['reviews'][0].update(state='CHANGES_REQUESTED'),
            'blocking_marker': lambda d: d['reviews'].append({'id': 2, 'user': {'login': 'codesolarlght'}, 'body': 'Changes recommended', 'state': 'COMMENTED'}),
            'no_verification': lambda d: d.update(comments=[]),
            'old_verification': lambda d: d['comments'][0].update(body=marker(sha='b' * 40)),
            'self': lambda d: d['comments'][0].update(body=marker(reviewer='author')),
            'failed_verification': lambda d: d['comments'].append({'id': 2, 'user': {'login': 'soilSpoon'}, 'body': marker(verdict='fail')}),
            'no_author': lambda d: d['pr'].update(body=''),
            'ambiguous_author': lambda d: d['pr'].update(body='Author-Session: a\nAuthor-Session: b'),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                data = fixture()
                mutate(data)
                self.assertTrue(evaluate(data, ['check'], POLICY), name)

    def test_latest_review_and_marker_win(self):
        data = fixture()
        data['reviews'] += [
            {'id': 2, 'user': {'login': 'codesolarlght'}, 'body': 'Changes recommended', 'state': 'COMMENTED'},
            {'id': 3, 'user': {'login': 'codesolarlght'}, 'body': 'Fixed', 'state': 'APPROVED'},
            {'id': 4, 'user': {'login': 'copilot'}, 'body': 'quota exceeded', 'state': 'CHANGES_REQUESTED'},
        ]
        data['comments'] += [{'id': 2, 'body': marker(verdict='fail')},
                             {'id': 3, 'body': marker()},
                             {'id': 4, 'body': marker(sha='b' * 40, verdict='fail')}]
        for comment in data['comments']:
            comment['user'] = {'login': 'soilSpoon'}
        self.assertEqual(evaluate(data, ['check'], POLICY), [])

    def test_rest_pagination(self):
        with patch('merge_gate.gh', side_effect=[[{'id': i} for i in range(100)], [{'id': 100}]]) as api:
            self.assertEqual(len(pages('endpoint')), 101)
            self.assertIn('page=2', api.call_args.args[0])

    def test_fetch_thread_pagination_and_head_race(self):
        def run(changed=False):
            calls = []
            def api(*args):
                calls.append(args)
                if args[0] == 'repos/{owner}/{repo}':
                    return {'full_name': 'owner/repo', 'default_branch': 'main'}
                if args[0] == 'repos/owner/repo/pulls/1':
                    pr = fixture()['pr']
                    if changed and sum(c[0] == args[0] for c in calls) > 1:
                        pr['head']['sha'] = 'b' * 40
                    return pr
                if args[0] == 'graphql':
                    second = 'cursor=next' in args
                    return {'data': {'repository': {'pullRequest': {'reviewThreads': {
                        'nodes': [{'isResolved': not second}],
                        'pageInfo': {'hasNextPage': not second, 'endCursor': 'next'}}}}}}
                if 'check-runs' in args[0]:
                    return {'check_runs': []}
                return []
            with patch('merge_gate.gh', side_effect=api):
                return fetch(1)
        self.assertEqual(run()['threads'], [{'isResolved': True}, {'isResolved': False}])
        with self.assertRaisesRegex(ValueError, 'head changed'):
            run(changed=True)

    def test_api_error_exits_closed(self):
        with patch('sys.argv', ['merge_gate.py', '1']), patch('merge_gate.fetch', side_effect=OSError('offline')):
            self.assertEqual(main(), 1)


if __name__ == '__main__':
    unittest.main()
