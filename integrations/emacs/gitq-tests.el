;;; gitq-tests.el --- Tests for the gitq Emacs integration  -*- lexical-binding: t; -*-

;;; Commentary:

;; Run with:
;;
;;   emacs --batch -l integrations/emacs/gitq.el \
;;                 -l integrations/emacs/gitq-tests.el \
;;                 -f ert-run-tests-batch-and-exit
;;
;; or `make test`, which does the same.
;;
;; These cover the parts of the integration that are pure: the rendering
;; helpers, the buffer-position arithmetic behind entry movement and region
;; selection, and the query strings those produce.  Nothing here shells out
;; to git or to the gitq binary — the binary has its own suite, and the point
;; of these is the elisp that has none.
;;
;; Several assert the *same* answers the Rust side produces for the same
;; input.  `gitq--stats-line' and `gitq--selection-step' are second
;; implementations of specs that live in Rust (`preview_stats',
;; `selection_step'), and two implementations of one spec drift silently
;; unless something pins them together.

;;; Code:

(require 'ert)

;;; --- rendering helpers -----------------------------------------------

(ert-deftest gitq-test-stats-hunks ()
  "Hunk stats count files, commits and line deltas.
Mirrors `preview_stats' in src/complete_tui.rs."
  (let ((hunks (list '(:type hunk :path "a.rs" :commit-sha "s1"
                       :content "+one\n-two\n ctx\n")
                     '(:type hunk :path "a.rs" :commit-sha "s2"
                       :content "+three\n")
                     '(:type hunk :path "b.rs" :commit-sha "s2"
                       :content "-four\n"))))
    (let ((s (gitq--stats-line hunks)))
      (should (string-match-p "3 hunks" s))
      (should (string-match-p "2 files" s))
      (should (string-match-p "2 commits" s))
      ;; a context line counts as neither
      (should (string-match-p (regexp-quote "+2 -2") s)))))

(ert-deftest gitq-test-stats-commits ()
  "Commit stats report authors and the date span."
  (let ((cs (list '(:type commit :author "alice" :date "2026-07-01 10:00:00 +0000")
                  '(:type commit :author "bob"   :date "2026-07-05 10:00:00 +0000"))))
    (let ((s (gitq--stats-line cs)))
      (should (string-match-p "2 commits" s))
      (should (string-match-p "2 authors" s))
      (should (string-match-p (regexp-quote "2026-07-01..2026-07-05") s)))))

(ert-deftest gitq-test-stats-singulars-and-empty ()
  "One of a thing reads as one, and a single day does not repeat itself."
  (let ((s (gitq--stats-line '((:type commit :author "a" :date "2026-07-01 09:00:00 +0000")))))
    (should (string-match-p "1 commit\\b" s))
    (should-not (string-match-p "1 commits" s))
    (should (string-match-p "2026-07-01" s))
    (should-not (string-match-p (regexp-quote "..") s)))
  (should-not (gitq--stats-line nil)))

;;; --- selection steps -------------------------------------------------

(ert-deftest gitq-test-selection-step ()
  "Contiguous rows collapse into half-open runs.
Mirrors `selection_step' in src/complete_tui.rs, which the terminal
completer emits for the same selections."
  (should (equal (gitq--selection-step '(0 1 2)) "[0..3]"))
  (should (equal (gitq--selection-step '(0 1 2 4 5)) "[0..3,4..6]"))
  (should (equal (gitq--selection-step '(5)) "[5]"))
  ;; unsorted input still produces ascending runs
  (should (equal (gitq--selection-step '(8 1 2)) "[1..3,8]"))
  (should-not (gitq--selection-step nil)))

;;; --- buffer arithmetic ------------------------------------------------

(defun gitq-tests--framed-buffer ()
  "A buffer with three result frames, the middle one multi-line."
  (let ((buf (generate-new-buffer " *gitq-test*")))
    (with-current-buffer buf
      (dolist (spec '((0 . "one\n") (1 . "two\nbody\nbody2\n") (2 . "three\n")))
        (let ((start (point)))
          (insert (cdr spec))
          (put-text-property start (point) 'gitq-frame (list :i (car spec))))))
    buf))

(ert-deftest gitq-test-frame-starts ()
  "Frame starts are found in result order, one per frame."
  (let ((buf (gitq-tests--framed-buffer)))
    (unwind-protect
        (with-current-buffer buf
          ;; "one\n" is 4 chars, "two\nbody\nbody2\n" is 15
          (should (equal (gitq--frame-starts) '(1 5 20))))
      (kill-buffer buf))))

(ert-deftest gitq-test-frame-index-at ()
  "Every position inside a frame reports that frame, body rows included."
  (let ((buf (gitq-tests--framed-buffer)))
    (unwind-protect
        (with-current-buffer buf
          (should (equal (gitq--frame-index-at 1) 0))
          ;; a row of the middle frame's body, not its first line
          (should (equal (gitq--frame-index-at 10) 1))
          ;; still inside the middle frame's body, not the next frame
          (should (equal (gitq--frame-index-at 16) 1))
          (should (equal (gitq--frame-index-at 20) 2)))
      (kill-buffer buf))))

(ert-deftest gitq-test-entry-movement-skips-multi-line-frames ()
  "Movement is by entry: a hunk is one stop however many rows it takes."
  (let ((buf (gitq-tests--framed-buffer)))
    (unwind-protect
        (with-current-buffer buf
          (goto-char (point-min))
          (should (equal (gitq--frame-index-at (point)) 0))
          (gitq-results-next-entry)
          (should (equal (gitq--frame-index-at (point)) 1))
          ;; the next stop is frame 2, not row 2 of frame 1's body
          (gitq-results-next-entry)
          (should (equal (gitq--frame-index-at (point)) 2))
          ;; past the end, point stays put rather than drifting
          (gitq-results-next-entry)
          (should (equal (gitq--frame-index-at (point)) 2))
          (gitq-results-previous-entry)
          (should (equal (gitq--frame-index-at (point)) 1))
          (gitq-results-previous-entry)
          (gitq-results-previous-entry)
          (should (equal (gitq--frame-index-at (point)) 0)))
      (kill-buffer buf))))

(ert-deftest gitq-test-rows-in-region ()
  "A frame counts as covered when the region touches any part of it."
  (let ((buf (gitq-tests--framed-buffer)))
    (unwind-protect
        (with-current-buffer buf
          ;; spanning the first two
          (should (equal (gitq--rows-in-region 1 8) '(0 1)))
          ;; entirely inside the middle frame's body
          (should (equal (gitq--rows-in-region 8 10) '(1)))
          (should (equal (gitq--rows-in-region (point-min) (point-max)) '(0 1 2))))
      (kill-buffer buf))))

(ert-deftest gitq-test-refine-of-one-entry-is-positional ()
  "A single entry refines by index, not by re-derived identity.
The identity form produced `SHA via diff.hunks where path == \"x\",
start-line == 20', which says what `[1]' says and breaks when a frame
lacks a field it pins on."
  (let ((buf (gitq-tests--framed-buffer)))
    (unwind-protect
        (with-current-buffer buf
          (goto-char (point-min))
          (gitq-results-next-entry)
          (should (equal (gitq--selection-step
                          (list (gitq--frame-index-at (point))))
                         "[1]")))
      (kill-buffer buf))))

(provide 'gitq-tests)
;;; gitq-tests.el ends here
