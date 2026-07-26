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

(ert-deftest gitq-test-each-search-term-gets-its-own-face ()
  "Intersecting greps are told apart by colour, as in the terminal.
Successive `grep' steps intersect, so a visible row matched all of them;
painting them alike hides which is which."
  (with-temp-buffer
    (let ((gitq--active-highlights
           (gitq--highlight-regexps "hunks grep dgcl grep along")))
      (should (= (length gitq--active-highlights) 2))
      (insert "a dgcl and along here\n")
      (gitq--apply-highlights (point-min) (point-max))
      (goto-char (point-min))
      (search-forward "dgcl")
      (let ((first (get-text-property (1- (point)) 'face)))
        (search-forward "along")
        (let ((second (get-text-property (1- (point)) 'face)))
          (should first)
          (should second)
          (should-not (equal first second)))))))

(ert-deftest gitq-test-match-faces-cycle ()
  "The palette wraps rather than running out."
  (should (equal (gitq--match-face 0) gitq-match-face))
  (should (equal (gitq--match-face 1) (nth 0 gitq-match-faces)))
  (should (equal (gitq--match-face (1+ (length gitq-match-faces)))
                 (nth 0 gitq-match-faces)))
  ;; with the palette emptied, everything falls back to one face
  (let ((gitq-match-faces nil))
    (should (equal (gitq--match-face 3) gitq-match-face))))

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

;;; --- buffer naming ---------------------------------------------------

(ert-deftest gitq-test-buffer-name ()
  "Results are named after their query; the preview is not."
  (should (equal (gitq--buffer-name "commits") "*gitq: commits*"))
  (should (equal (gitq--buffer-name "hunks grep widget")
                 "*gitq: hunks grep widget*"))
  ;; the preview re-renders per keystroke, so it shares one buffer
  (should (equal (gitq--buffer-name "hunks grep widget" t) "*gitq*"))
  ;; an empty query has nothing to name itself after
  (should (equal (gitq--buffer-name "") "*gitq*")))

(ert-deftest gitq-test-buffer-name-elides-long-queries ()
  "A long pipeline is elided in the middle, keeping both ends."
  (let* ((long (concat "commits where message \"" (make-string 100 ?x) "\" /count"))
         (name (gitq--buffer-name long)))
    (should (< (length name) (+ gitq-buffer-name-max 12)))
    (should (string-match-p "\\`\\*gitq: commits" name))
    (should (string-match-p "count\\*\\'" name))
    (should (string-match-p "…" name))))

(ert-deftest gitq-test-each-query-gets-its-own-buffer ()
  "Accepting a query adds a buffer rather than replacing the last one.
Two results can then sit side by side; re-running the same query reuses
its buffer rather than accumulating duplicates; and the live preview stays
in one shared buffer, since it re-renders per keystroke."
  (let ((one '((:type commit :sha "aaa" :author "a" :date "2026-07-01 x" :message "one")))
        (two '((:type commit :sha "bbb" :author "b" :date "2026-07-02 x" :message "two")))
        (made nil))
    (unwind-protect
        (progn
          (push (gitq--render one "commits [0]") made)
          (push (gitq--render two "hunks grep widget") made)
          (push (gitq--render one "commits [0]") made)      ; same query again
          (push (gitq--render two "a preview" nil t) made)  ; preview
          (let ((names (seq-filter (lambda (n) (string-prefix-p "*gitq" n))
                                   (mapcar #'buffer-name (buffer-list)))))
            (should (member "*gitq: commits [0]*" names))
            (should (member "*gitq: hunks grep widget*" names))
            (should (member "*gitq*" names))
            ;; the repeat did not make a second copy
            (should (= 1 (seq-count (lambda (n) (equal n "*gitq: commits [0]*")) names)))))
      (dolist (b (delete-dups made))
        (when (buffer-live-p b) (kill-buffer b))))))

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

(ert-deftest gitq-test-motion-keys-reach-entry-movement ()
  "The movement keys must actually resolve to entry movement.
Binding them in the major-mode map is not enough under evil: `j', `k' and
the arrows live in evil's motion-state map, which is consulted first, so
the keys were bound and `evil-next-line' ran anyway."
  (with-temp-buffer
    (gitq-results-mode)
    (when (fboundp 'evil-motion-state) (evil-motion-state))
    (dolist (k '("j" "n"))
      (should (eq (key-binding k) 'gitq-results-next-entry)))
    (dolist (k '("k" "p"))
      (should (eq (key-binding k) 'gitq-results-previous-entry)))
    (should (eq (key-binding [down]) 'gitq-results-next-entry))
    (should (eq (key-binding [up]) 'gitq-results-previous-entry))
    ;; and the actions are not swallowed either
    (should (eq (key-binding (kbd "TAB")) 'gitq-results-refine))))

(ert-deftest gitq-test-movement-commands-are-evil-motions ()
  "Entry movement must be declared a motion, or visual state ends on it.
Evil's post-command hook exits the selection after a command without
`:keep-visual', so in visual state the keys did nothing visible at all —
`evil-next-line' carries the property for the same reason."
  (skip-unless (fboundp 'evil-get-command-property))
  (dolist (cmd '(gitq-results-next-entry gitq-results-previous-entry))
    (should (evil-get-command-property cmd :keep-visual))))

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
