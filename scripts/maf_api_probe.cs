#:package Microsoft.Agents.AI@1.23.0
#:package Microsoft.Agents.AI.Harness@1.23.0
#:property PublishAot=false

using System;
using System.Linq;
using Microsoft.Agents.AI;

Type[] types =
[
    typeof(AIAgent),
    typeof(AgentSession),
    typeof(AgentSkill),
    typeof(HarnessAgent),
    typeof(AgentSkillsProvider),
    typeof(AgentFileSkillsSource),
    typeof(AgentSkillsProviderBuilder)
];

foreach (Type type in types)
{
    Console.WriteLine($"{type.FullName} | {type.Assembly.GetName().Version}");
    foreach (var method in type.GetMethods().Where(method =>
        method.DeclaringType == type &&
        (method.Name.Contains("Skills") || method.Name.Contains("Session") ||
         method.Name.Contains("Content") || method.Name.Contains("Resource") ||
         method.Name.Contains("Script"))))
    {
        Console.WriteLine($"  {method}");
    }
}

(string Name, string Description, bool ExpectedAccepted)[] cases =
[
    ("valid-skill", "Compatibility probe.", true),
    ("Legacy_Skill", "Compatibility probe.", false),
    ("skill with spaces", "Compatibility probe.", false),
    ("long-description", new string('x', 1025), false)
];

foreach (var testCase in cases)
{
    bool accepted;
    try
    {
        _ = new AgentSkillFrontmatter(testCase.Name, testCase.Description);
        accepted = true;
    }
    catch (ArgumentException)
    {
        accepted = false;
    }

    if (accepted != testCase.ExpectedAccepted)
    {
        throw new InvalidOperationException(
            $"Unexpected metadata validation for {testCase.Name}: accepted={accepted}");
    }

    Console.WriteLine($"PASS {testCase.Name}: accepted={accepted}");
}

Console.WriteLine("MAF API and metadata probe passed; no model or script execution performed.");