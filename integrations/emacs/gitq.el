;;; gitq.el --- Emacs front-end for the gitq CLI  -*- lexical-binding: t; -*-

;; Author: baleti
;; URL: https://github.com/baleti/gitq
;; Package-Requires: ((emacs "28.1"))
;; Version: 0.1.0

;;; Commentary:

;; Emacs front-end for gitq, the typed, categorical query language for
;; git.  All parsing, type checking, completion candidates, and execution
;; live in the `gitq' binary (the Haskell implementation in this repo);
;; this package provides the interactive Emacs experience on top of it:
;;
;; - `M-x gitq': minibuffer entry with context-aware completion for the
;;   current pipeline position (candidates and their annotations come from
;;   `gitq --complete-annotated', so they can never drift from the parser),
;;   plus a debounced live preview of results while typing.
;; - a results buffer (`gitq-results-mode') with RET to visit the object
;;   at point (via magit when available), `b' to branch off, `c' to copy
;;   the SHA, `q' to quit.
;; - re-invoking `gitq' from a results buffer pre-fills the minibuffer
;;   with the pipeline that produced it.
;;
;; Pipelines whose terminal is effectful (/branch-off, /commit, ...) run
;; through the CLI so the effect happens exactly as documented there;
;; /show and terminal-less pipelines render into the results buffer.

;;; Code:

(require 'cl-lib)

(defgroup gitq nil
  "Emacs front-end for the gitq CLI."
  :group 'tools)

(defcustom gitq-executable "gitq"
  "Path to the gitq binary."
  :type 'string :group 'gitq)

(defcustom gitq-preview-debounce 0.2
  "Seconds of no further input change before gitq (re)previews results."
  :type 'number :group 'gitq)

(defcustom gitq-release-url "https://github.com/baleti/gitq/releases/latest/download/"
  "Base URL prebuilt gitq binaries are downloaded from.
Assets are published by the repo's release workflow (one per platform,
e.g. \"gitq-x86_64-linux\")."
  :type 'string :group 'gitq)

(defcustom gitq-install-directory (expand-file-name "~/.local/bin/")
  "Directory `gitq-install-binary' downloads the gitq binary into."
  :type 'directory :group 'gitq)

;;; Running the CLI

(defun gitq--release-asset ()
  "Release asset name for this platform, or nil if none is published."
  (pcase (list system-type (car (split-string system-configuration "-")))
    (`(gnu/linux "x86_64") "gitq-x86_64-linux")
    (_ nil)))

(defun gitq-install-binary ()
  "Download the prebuilt gitq binary from the latest GitHub release.
Installs into `gitq-install-directory' and returns the binary's path.
The assets are built by the repo's release workflow; platforms without
a published asset must build from source (make install-native)."
  (interactive)
  (let ((asset (gitq--release-asset)))
    (unless asset
      (user-error "gitq: no prebuilt binary for %s/%s — build from source: cd <gitq repo> && make install-native"
                  system-type system-configuration))
    (let ((url (concat gitq-release-url asset))
          (dest (expand-file-name "gitq" gitq-install-directory)))
      (make-directory gitq-install-directory t)
      (message "gitq: downloading %s ..." url)
      (url-copy-file url dest t)
      (set-file-modes dest #o755)
      (message "gitq: installed %s" dest)
      dest)))

(defun gitq--ensure-executable ()
  "Ensure the gitq binary is available, offering to download a release.
Called up front by `gitq' so a missing binary is one actionable prompt,
not a `file-missing' stack trace inside the completion UI on the first
keystroke.  Accepting downloads via `gitq-install-binary'; if the
install directory isn't on `exec-path', `gitq-executable' is pointed at
the downloaded file directly."
  (unless (executable-find gitq-executable)
    (if (and (gitq--release-asset)
             (y-or-n-p (format "gitq binary '%s' not found; download the prebuilt release binary to %s? "
                               gitq-executable gitq-install-directory)))
        (let ((dest (gitq-install-binary)))
          (unless (executable-find gitq-executable)
            (setq gitq-executable dest)))
      (user-error "gitq: binary '%s' not found on `exec-path' — M-x gitq-install-binary, or build from source (cd ~/gitq && make install-native), or set `gitq-executable'"
                  gitq-executable))))

(defun gitq--run (&rest args)
  "Run the gitq binary with ARGS; return (EXIT-CODE . OUTPUT)."
  (with-temp-buffer
    (let ((code (apply #'call-process gitq-executable nil (list t nil) nil args)))
      (cons code (buffer-string)))))

(defun gitq--frames (pipeline)
  "Execute PIPELINE's source and steps (never its terminal); return frames.
Frames are plists as printed by `gitq --sexp'.  Signals a `user-error'
with the CLI's message on failure."
  (pcase-let ((`(,code . ,out) (gitq--run "--sexp" "--preview" pipeline)))
    (if (zerop code)
        (car (read-from-string (concat "(" out ")")))
      (user-error "%s" (string-trim out)))))

;;; Results buffer

(defvar gitq-results-mode-map
  (let ((m (make-sparse-keymap)))
    (define-key m (kbd "RET") #'gitq-results-visit)
    (define-key m (kbd "b")   #'gitq-results-branch-off)
    (define-key m (kbd "c")   #'gitq-results-copy-sha)
    (define-key m (kbd "q")   #'quit-window)
    m)
  "Keymap for `gitq-results-mode'.")

(define-derived-mode gitq-results-mode special-mode "GitQ"
  "Major mode for displaying gitq pipeline results."
  :interactive nil
  (setq truncate-lines t))

(defvar-local gitq--buffer-pipeline nil
  "The pipeline string that produced this `gitq-results-mode' buffer.
Set by `gitq--render' after the major mode is turned on (major modes
call `kill-all-local-variables', which would otherwise wipe it).")

(defun gitq--face (magit-face fallback)
  "Return MAGIT-FACE if it is defined, else FALLBACK."
  (if (facep magit-face) magit-face fallback))

(defun gitq--frame-commit-sha (frame)
  "Return the commit SHA for FRAME (direct or via :commit-sha)."
  (or (plist-get frame :commit-sha)
      (plist-get frame :sha)))

(defun gitq--insert-frame (frame)
  "Insert a human-readable line for FRAME into the current buffer."
  (let ((type  (plist-get frame :type))
        (start (point)))
    (pcase type
      ('commit
       (let* ((sha   (plist-get frame :sha))
              (short (when sha (substring sha 0 (min 8 (length sha))))))
         (insert (propertize (or short "?")
                             'face (gitq--face 'magit-hash 'shadow)))
         (insert "  ")
         (let ((author (plist-get frame :author)))
           (when author
             (insert (propertize
                      (format "%-20s"
                              (substring author 0 (min 20 (length author))))
                      'face (gitq--face 'magit-log-author 'font-lock-string-face)))))
         (let ((date (plist-get frame :date)))
           (when date
             (insert (propertize (substring date 0 (min 10 (length date)))
                                 'face (gitq--face 'magit-log-date 'shadow)))
             (insert "  ")))
         (insert (or (plist-get frame :message) ""))))
      ('blob
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face))))
      ('ref
       (insert (propertize (or (plist-get frame :name) "?")
                           'face (gitq--face 'magit-branch-local 'font-lock-keyword-face)))
       (when-let ((sha (plist-get frame :sha)))
         (insert "  ")
         (insert (propertize (substring sha 0 (min 8 (length sha)))
                             'face (gitq--face 'magit-hash 'shadow)))))
      ('worktree
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
       (when-let ((b (plist-get frame :branch)))
         (insert "  ")
         (insert (propertize b 'face (gitq--face 'magit-branch-local 'font-lock-keyword-face)))))
      ('line
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
       (insert ":")
       (insert (propertize (number-to-string (or (plist-get frame :line-number) 0))
                           'face 'shadow))
       (insert ": ")
       (insert (or (plist-get frame :content) "")))
      ('hunk
       (insert (propertize (or (plist-get frame :path) "?")
                           'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
       (insert (format " lines %d-%d"
                       (or (plist-get frame :start-line) 0)
                       (or (plist-get frame :end-line) 0))))
      ('diff-line
       (let* ((sign  (or (plist-get frame :sign) "?"))
              (added (equal sign "+")))
         (when-let ((csha (plist-get frame :commit-sha)))
           (insert (propertize (substring csha 0 (min 8 (length csha)))
                               'face (gitq--face 'magit-hash 'shadow)))
           (insert "  "))
         (insert (propertize (or (plist-get frame :path) "?")
                             'face (gitq--face 'magit-filename 'font-lock-function-name-face)))
         (insert ":")
         (insert (propertize (number-to-string (or (plist-get frame :line-number) 0))
                             'face 'shadow))
         (insert ": ")
         (insert (propertize (concat sign (or (plist-get frame :content) ""))
                             'face (if added 'diff-added 'diff-removed)))))
      (_
       ;; projected or unknown — dump key:value pairs
       (cl-loop for (k v) on frame by #'cddr
                do (insert (format "%s:%s " k v)))))
    (put-text-property start (point) 'gitq-frame frame)
    (put-text-property start (point) 'gitq-sha (gitq--frame-commit-sha frame))
    (insert "\n")))

(defun gitq--render (frames pipeline-str)
  "Render FRAMES into the *gitq* results buffer and return that buffer."
  (with-current-buffer (get-buffer-create "*gitq*")
    (let ((inhibit-read-only t))
      (erase-buffer)
      (insert (propertize (format "gitq: %s\n\n" pipeline-str)
                          'face 'font-lock-comment-face))
      (if frames
          (dolist (f frames) (gitq--insert-frame f))
        (insert "(no results)\n"))
      (gitq-results-mode)
      (setq gitq--buffer-pipeline pipeline-str)
      (goto-char (point-min)))
    (current-buffer)))

(defun gitq--display (frames pipeline-str)
  "Show FRAMES in the *gitq* buffer, taking over the whole frame."
  (pop-to-buffer (gitq--render frames pipeline-str) '(display-buffer-full-frame)))

(defun gitq--preview-display (frames pipeline-str)
  "Show FRAMES in the *gitq* buffer without selecting its window.
`display-buffer-full-frame' works via `delete-other-windows', which
selects its target window as a side effect — during a minibuffer read
that silently steals focus from typing, so the previously selected
window is restored explicitly."
  (let ((previously-selected (selected-window)))
    (display-buffer (gitq--render frames pipeline-str) '(display-buffer-full-frame))
    (when (window-live-p previously-selected)
      (select-window previously-selected))))

(defun gitq-results-visit ()
  "Visit the git object at point in the *gitq* buffer."
  (interactive nil gitq-results-mode)
  (let* ((frame (get-text-property (point) 'gitq-frame))
         (sha   (get-text-property (point) 'gitq-sha))
         (type  (plist-get frame :type)))
    (pcase type
      ('blob (when (and sha (fboundp 'magit-find-file))
               (magit-find-file sha (plist-get frame :path))))
      (_     (when (and sha (fboundp 'magit-show-commit))
               (magit-show-commit sha))))))

(defun gitq-results-branch-off ()
  "Create a branch from the commit at point in the *gitq* buffer."
  (interactive nil gitq-results-mode)
  (let ((sha (get-text-property (point) 'gitq-sha)))
    (unless sha (user-error "No commit at point"))
    (let ((name (read-string "Branch name: ")))
      (call-process "git" nil nil nil "checkout" "-b" name sha)
      (when (fboundp 'magit-refresh) (magit-refresh))
      (message "gitq: created branch '%s'" name))))

(defun gitq-results-copy-sha ()
  "Copy the SHA at point to the kill ring."
  (interactive nil gitq-results-mode)
  (let ((sha (get-text-property (point) 'gitq-sha)))
    (if sha
        (progn (kill-new sha)
               (message "gitq: copied %s" (substring sha 0 (min 8 (length sha)))))
      (user-error "No SHA at point"))))

;;; Completion

(defun gitq--complete-candidates (input)
  "Return (CANDIDATE . DESCRIPTION) pairs for the pipeline string INPUT.
Delegates to `gitq --complete-annotated', the same engine the strict
parser is built on, so completion can never offer a token the parser
rejects.  Never signals — completion runs on every keystroke inside the
completion UI, so any failure (binary missing, not a repo) just means
no candidates; `gitq--ensure-executable' reports the actionable error
once, at the prompt."
  (condition-case nil
      (pcase-let ((`(,code . ,out) (gitq--run "--complete-annotated" input)))
        (when (zerop code)
          (mapcar (lambda (line)
                    (pcase-let ((`(,cand ,desc) (split-string line "\t")))
                      (cons cand (or desc ""))))
                  (split-string out "\n" t))))
    (error nil)))

(defun gitq--current-token (string)
  "Return the in-progress partial token at the end of STRING, or \"\"."
  (if (string-match "\\(?:^\\|[ \t]\\)\\([^ \t]*\\)$" string)
      (match-string 1 string)
    ""))

(defvar gitq--completion-cache nil
  "Cons of (INPUT . CANDIDATES) memoizing the last CLI completion call.")

(defun gitq--candidates-for (string)
  "Candidates (with descriptions) for STRING, memoized per input."
  (unless (equal (car gitq--completion-cache) string)
    (setq gitq--completion-cache
          (cons string (gitq--complete-candidates string))))
  (cdr gitq--completion-cache))

(defun gitq--affixate (candidates)
  "Return CANDIDATES as (CAND \"\" DESC) triples for completion UIs."
  (let ((table (cdr gitq--completion-cache)))
    (mapcar (lambda (c)
              (let ((desc (cdr (assoc c table))))
                (list c ""
                      (if (and desc (not (string-empty-p desc)))
                          (propertize (concat "  " desc)
                                      'face 'completions-annotations)
                        ""))))
            candidates)))

(defun gitq--completion-table (string predicate action)
  "Dynamic `completing-read' collection table for a growing gitq pipeline.
Only the in-progress final token of STRING is completed; earlier tokens
are fixed context."
  (cond
   ((eq action 'metadata)
    '(metadata (category . gitq-token)
               (affixation-function . gitq--affixate)))
   ((eq (car-safe action) 'boundaries)
    (cons 'boundaries
          (cons (- (length string) (length (gitq--current-token string))) 0)))
   (t
    (complete-with-action action
                          (mapcar #'car (gitq--candidates-for string))
                          (gitq--current-token string)
                          predicate))))

;;; Reading a pipeline, with live preview

(defvar gitq--history nil "Minibuffer history list for `gitq'.")

(defun gitq--history-search ()
  "Search `gitq--history' and splice the chosen pipeline into the minibuffer."
  (interactive)
  (unless gitq--history
    (user-error "No gitq history yet"))
  (let* ((enable-recursive-minibuffers t)
         (choice (completing-read "gitq history: " gitq--history nil t)))
    (delete-minibuffer-contents)
    (insert choice)))

(defvar gitq--pipeline-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-r") #'gitq--history-search)
    map)
  "Keymap merged into the minibuffer's local map by `gitq--read-pipeline'.")

(defun gitq--preview-frames (input)
  "Return (:ok . FRAMES) if INPUT previews cleanly, else nil.
Runs the CLI with --preview (a terminal keyword ends the pipeline but
its action is never applied).  Returns nil, with no side effects, if
INPUT does not currently parse or execute cleanly."
  (ignore-errors
    (cons :ok (gitq--frames input))))

(defun gitq--read-pipeline (prompt &optional initial-input)
  "Read a gitq pipeline with completion and a debounced live preview.
INITIAL-INPUT, if non-nil, pre-fills the minibuffer.  \\`C-r' opens a
history search over previously-run pipelines."
  (let ((mb-buffer  nil)
        (last-input nil)
        (timer      nil))
    (cl-labels
        ((tick ()
           (when (buffer-live-p mb-buffer)
             (with-current-buffer mb-buffer
               (let ((input (minibuffer-contents-no-properties)))
                 (unless (equal input last-input)
                   (setq last-input input)
                   (pcase (gitq--preview-frames input)
                     (`(:ok . ,frames) (gitq--preview-display frames input))))))))
         (schedule ()
           (when timer (cancel-timer timer))
           (setq timer (run-with-timer gitq-preview-debounce nil #'tick)))
         (setup ()
           (setq mb-buffer (current-buffer))
           (use-local-map (make-composed-keymap gitq--pipeline-map (current-local-map)))
           (add-hook 'post-command-hook #'schedule nil t)))
      (unwind-protect
          (minibuffer-with-setup-hook #'setup
            (completing-read prompt #'gitq--completion-table nil nil
                             initial-input 'gitq--history))
        (when timer (cancel-timer timer))))))

;;; Entry point

(defun gitq--effectful-terminal-p (pipeline)
  "Return non-nil if PIPELINE ends in a terminal other than /show.
Such pipelines run through the CLI so the effect happens there; /show
and terminal-less pipelines render into the results buffer instead."
  (let ((words (split-string pipeline)))
    (when-let ((last-word (car (last words))))
      (and (string-prefix-p "/" last-word)
           (not (string-suffix-p "/" last-word))
           (not (equal last-word "/show"))))))

;;;###autoload
(defun gitq (pipeline)
  "Execute a GitQ PIPELINE: a whitespace-separated query over git's object graph.

PIPELINE syntax:  source [step...] [/terminal]

Sources:   commits [in RANGE]  HEAD  BRANCH  branches  tags  refs  worktrees  blobs
Steps:     via MORPHISM-PATH  where COND[,COND...]  grep PATTERN  pickaxe PATTERN
           path GLOB  pick FIELD[,...]  take N  skip N  first  last  sort [-]FIELD
Terminals: /show  /copy  /insert  /count  /branch-off [NAME]  /amend [no-edit|MSG]
           /squash [MSG]  /reword [MSG]  /remove  /delete  /commit [MSG]
           /stage  /mark [LABEL]  /worktree [PATH]

See the gitq CLI's own documentation (doc/gitq.org) for the language;
run \\`gitq --help' for CLI usage.  Invoked from a `gitq-results-mode'
buffer, the minibuffer is pre-filled with the pipeline that produced it."
  (interactive
   (progn
     (gitq--ensure-executable)
     (list (gitq--read-pipeline "gitq> "
                                (when (derived-mode-p 'gitq-results-mode)
                                  gitq--buffer-pipeline)))))
  (gitq--ensure-executable)
  (if (gitq--effectful-terminal-p pipeline)
      (pcase-let ((`(,code . ,out) (gitq--run pipeline)))
        (if (zerop code)
            (progn
              (when (fboundp 'magit-refresh) (magit-refresh))
              (message "%s" (string-trim out)))
          (user-error "%s" (string-trim out))))
    (gitq--display (gitq--frames pipeline) pipeline)))

(provide 'gitq)
;;; gitq.el ends here
