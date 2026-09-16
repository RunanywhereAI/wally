package cmd

import (
	"reflect"
	"testing"
)

func TestSplitModelArgs(t *testing.T) {
	cases := []struct {
		args      []string
		wantModel string
		wantRest  []string
	}{
		{[]string{"--allow-dangerously-skip-permissions"}, "", []string{"--allow-dangerously-skip-permissions"}},
		{[]string{"glm-5.3-flash"}, "glm-5.3-flash", []string{}},
		{[]string{"glm-5.3-flash", "--foo", "bar"}, "glm-5.3-flash", []string{"--foo", "bar"}},
		{[]string{"--model", "x", "--foo"}, "x", []string{"--foo"}},
		{[]string{"--model=x", "--foo"}, "x", []string{"--foo"}},
		{[]string{}, "", []string{}},
	}
	for _, tc := range cases {
		gotModel, gotRest := splitModelArgs(tc.args)
		if gotModel != tc.wantModel || !reflect.DeepEqual(gotRest, tc.wantRest) {
			t.Errorf("splitModelArgs(%v) = (%q, %v), want (%q, %v)", tc.args, gotModel, gotRest, tc.wantModel, tc.wantRest)
		}
	}
}
